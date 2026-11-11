//! Polls over HTTP: the poll a status asks and the ballot a viewer casts
//! on it (`GET /api/v1/polls/{id}`, `POST /api/v1/polls/{id}/votes`).
//!
//! The reference is Pleroma's `PollController`
//! (`lib/pleroma/web/mastodon_api/controllers/poll_controller.ex`, routed
//! `get "/polls/:id"` and `post "/polls/:id/votes"` in
//! `lib/pleroma/web/router.ex`) over `CommonAPI.vote/3`
//! (`lib/pleroma/web/common_api.ex`). Pleroma keys both routes on the
//! poll's *object* id and records each mark as an `Answer` activity;
//! here the poll is its own row and a ballot is one transaction over
//! `poll_votes` (decision 6), so the routes key on the [`polls`] row's
//! own id and the write moves the stored counters instead of rewriting
//! the post. The gate is Pleroma's: the read runs with `read:statuses`
//! and `fallback: :proceed_unauthenticated` — [`MaybeAuthUser`], whose
//! [`require_user_scope`] refuses a presented user token that lacks the
//! scope while an anonymous request proceeds — and the vote is an
//! [`AuthUser`] action on `write:statuses`, riding the
//! `statuses_actions` throttle bucket Pleroma's rate-limit config
//! counts poll voting with (see [`crate::rate_limit`]).
//!
//! [`polls`]: crate::models::poll
//! [`require_user_scope`]: crate::auth::MaybeAuthUser::require_user_scope
//!
//! Both routes resolve the poll through the same gate: the id must parse
//! and name a stored poll whose *status* the viewer may see — Pleroma's
//! `Visibility.visible_for_user?(activity, user)` in front of the
//! render — and a malformed, unknown or invisible one is the shared 404
//! `{"error":"Record not found"}` (this module's `poll_for_viewer_or_404`),
//! which never reveals that a private question exists. Behind it the answer is
//! the [`Poll`] entity built for *this* viewer: `voted` and `own_votes`
//! are the caller's marks, and the `hide_totals` rule — per-option
//! counts withheld as JSON `null` while the poll runs, shown once it
//! closes — is the entity's, lifted at the deadline it shares with the
//! `expired` flag.
//!
//! What the vote refuses, all as 422 bodies worded per the ticket (the
//! wording Mastodon's `PollVoteService` and Pleroma's `CommonAPI.vote`
//! answer with), in this order:
//!
//! - the deadline has passed: `"Poll's finished"`;
//! - the voter already marked anything on this poll:
//!   `"You have already voted on this poll"`;
//! - the caller authored the poll's status: `"Poll's author can't vote"`
//!   (Pleroma's `validate_not_author`, which it runs before its own
//!   already-voted check);
//! - the body carries no choices, as a missing key, JSON `null` or the
//!   empty array: `"Missing parameter: choices"`;
//! - a requested index names no option of the poll — negative, past the
//!   last one, or a position no row holds: `"Invalid indices"`;
//! - more than one distinct index on a single-choice poll:
//!   `"Too many choices"`.
//!
//! Duplicate indexes are ignored, not refused: one mark per distinct
//! option, which is the primary key's own shape. Everything a
//! request that passes writes is one transaction ([`poll::cast_vote`]):
//! the voter's `poll_votes` rows, each chosen option's `votes_count`
//! stepped, the poll's `votes_count` by the number of marks and its
//! `voters_count` by one — and the answer is the updated [`Poll`] entity
//! with `voted` true and the cast positions in `own_votes`.
//!
//! The vote's `choices` arrives through [`ApiBody`] (decision 10): a JSON
//! array of integers (`{"choices": [0, 1]}`), the form's repeated
//! `choices[]=0&choices[]=1` — whose values are strings and parse like
//! the numbers they spell, as Pleroma's `String.to_integer` arm accepts
//! them — a lone `choices=0`, and the indexed `choices[0]=1` spelling,
//! all deserializing to the same list. A value that is no integer at all
//! is the extractor's own 422, before any poll is looked up.
//!
//! The telling of the poll's author and the federation of the answer
//! activities the ballot becomes are later tickets: they land in this
//! module's `after_vote_cast` hook, the documented empty point the way
//! [`crate::api::reports`] leaves `after_report_created`.

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, de::Error as SerdeError};
use serde_json::Value;
use tracing::debug;
use uuid::Uuid;

use crate::{
    auth::{AuthUser, MaybeAuthUser},
    entities::poll::Poll,
    error::{AppError, AppResult},
    extract::{ApiBody, index_map_to_vec},
    models::{
        poll::{self, Poll as PollRow, PollOption as PollOptionRow},
        status::{self, Status as StatusRow},
        status_visibility,
        user::User,
    },
    oauth::scopes::{READ_STATUSES, WRITE_STATUSES},
    state::AppState,
    util::id::parse_id,
};

/// 422 body of `POST /api/v1/polls/{id}/votes` for a poll whose deadline
/// has passed (Mastodon's `PollExpiredError` wording).
const POLL_FINISHED: &str = "Poll's finished";
/// 422 body for a voter who already marked this poll (Mastodon's
/// `PollAlreadyVotedError` wording; Pleroma's `validate_existing_votes`
/// says the shorter "Already voted").
const ALREADY_VOTED: &str = "You have already voted on this poll";
/// 422 body for the author of the poll's status (Pleroma's
/// `validate_not_author` wording, which Mastodon shares).
const AUTHOR_VOTE: &str = "Poll's author can't vote";
/// 422 body for a vote body that carries no choices at all (Pleroma's
/// `CastAndValidate` renders a missing required array exactly this way).
const MISSING_CHOICES: &str = "Missing parameter: choices";
/// 422 body for a requested index that names no option of the poll
/// (Pleroma's `normalize_and_validate_choices` `valid_choice` arm).
const INVALID_INDICES: &str = "Invalid indices";
/// 422 body for a second distinct choice on a single-choice poll
/// (Pleroma's `count_check` arm).
const TOO_MANY_CHOICES: &str = "Too many choices";

/// The `/api/v1` polls router, merged into [`crate::api::v1`].
///
/// `state` is what lets the vote route mount the `statuses_actions`
/// rate limiter the way the reports and status-action routes do (see
/// [`crate::rate_limit`]); the plain read rides no bucket, Pleroma's
/// `PollController.show` throtles nothing.
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new().route("/polls/{id}", get(show_poll)).route(
        "/polls/{id}/votes",
        post(cast_poll_vote).layer(crate::rate_limit::rate_limit(state, "statuses_actions")),
    )
}

/// `GET /api/v1/polls/{id}` — the poll `id` names, rendered as the
/// [`Poll`] entity as the viewer behind an optional token sees it:
/// their marks in `voted` and `own_votes`, nobody else's ever shown.
///
/// The gate is Pleroma's `show` action: `read:statuses` on a presented
/// user token, anonymous otherwise ([`MaybeAuthUser`]), and the poll's
/// status judged by the shared [`status_visibility`] — an invisible or
/// unknown poll is the one 404 before anything of the poll renders.
///
/// # Errors
///
/// [`AppError::NotFound`] for a malformed, unknown or deleted poll id
/// and for a poll whose status the viewer may not see;
/// [`AppError::Forbidden`] when a presented user token lacks
/// `read:statuses`; [`AppError::Unauthorized`] from the extractor for a
/// token that does not resolve. Any database failure arrives through `?`
/// as [`AppError::Database`].
async fn show_poll(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Poll>> {
    auth.require_user_scope(READ_STATUSES)?;
    let viewer = auth.0.as_ref();
    let (poll, _posted) = poll_for_viewer_or_404(&state, viewer, &id).await?;
    render_poll(&state, &poll, viewer).await.map(Json)
}

/// `POST /api/v1/polls/{id}/votes` — cast the ballot the body describes
/// on the poll `id` names and answer the updated [`Poll`] entity, seen
/// by its new voter: `voted` true, the cast positions in `own_votes`,
/// every counter already moved.
///
/// The gate is Pleroma's `vote` action, then the refusals the ticket
/// pins — [`accepted_marks`] for each of them, run in order — and the
/// one transaction [`poll::cast_vote`] for what a passing ballot
/// writes. The already-voted check runs twice on purpose: cheaply here,
/// to order the refusals, and again under the poll's row lock inside the
/// transaction, so two racing requests from one account leave one
/// ballot and one 422, never two.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:statuses` check; [`AppError::RateLimited`] from the
/// bucket before the handler runs; [`AppError::NotFound`] for a
/// malformed, unknown, deleted or invisible poll;
/// [`AppError::Unprocessable`] for any refusal of [`accepted_marks`] or
/// the locked rerun of the already-voted one. Any database failure
/// arrives through `?` as [`AppError::Database`].
async fn cast_poll_vote(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(params): ApiBody<VoteParams>,
) -> AppResult<Json<Poll>> {
    auth.require(WRITE_STATUSES)?;
    let voter = &auth.0;
    let (poll, posted) = poll_for_viewer_or_404(&state, Some(voter), &id).await?;
    let chosen = accepted_marks(&state, &poll, &posted, voter, &params.choices).await?;
    if !poll::cast_vote(&state.db, poll.id, voter.id, &chosen).await? {
        return Err(AppError::Unprocessable(ALREADY_VOTED.to_owned()));
    }
    debug!(
        poll = %poll.id,
        voter = %voter.id,
        marks = chosen.len(),
        "poll voted"
    );
    let updated = poll::get_by_id(&state.db, poll.id)
        .await?
        .ok_or(AppError::NotFound)?;
    after_vote_cast(&state, &updated, voter, &chosen);
    render_poll(&state, &updated, Some(voter)).await.map(Json)
}

/// Resolves the `{id}` segment of a poll route to its [`PollRow`] and the
/// live [`StatusRow`] of the status asking it, as `viewer` may see them
/// (`None`: an anonymous request).
///
/// One gate for both routes, Pleroma's `with` chain of `PollController`
/// restated over the relational shape: the id must be `UUID` text
/// ([`parse_id`], decision 4), must name a stored poll, and the poll's
/// status must still exist and pass [`status_visibility::can_view`] for
/// this viewer. Every failure is the same [`AppError::NotFound`] —
/// malformed, unknown, deleted, invisible — so the endpoint never hints
/// that a private question exists.
///
/// # Errors
///
/// [`AppError::NotFound`] as described; any database failure arrives
/// through `?` as [`AppError::Database`].
async fn poll_for_viewer_or_404(
    state: &AppState,
    viewer: Option<&User>,
    id_str: &str,
) -> AppResult<(PollRow, StatusRow)> {
    let Some(id) = parse_id(id_str) else {
        return Err(AppError::NotFound);
    };
    let Some(poll) = poll::get_by_id(&state.db, id).await? else {
        return Err(AppError::NotFound);
    };
    // The statuses foreign key keeps a stored poll's status around while
    // the poll lives; a live read that comes back empty is the row a
    // delete just took — the tombstone behind it is invisible to everyone
    // anyway, rule 1 of the visibility model.
    let Some(posted) = status::get_by_id(&state.db, poll.status_id).await? else {
        return Err(AppError::NotFound);
    };
    if status_visibility::can_view(&state.db, &posted, viewer).await? {
        Ok((poll, posted))
    } else {
        Err(AppError::NotFound)
    }
}

/// Reads the poll `poll` apart and renders it as the [`Poll`] entity for
/// `viewer` (`None`: anonymously, which shows none of anybody's marks).
///
/// The entity's own rules — the strict `expires_at < now` of `expired`
/// and the `hide_totals` withholding that lifts at it — are decided at
/// [`Utc::now`], the instant this read answers for; the row travels in
/// because the caller has it, the options and the viewer's marks are the
/// two reads the entity still needs.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn render_poll(state: &AppState, poll: &PollRow, viewer: Option<&User>) -> AppResult<Poll> {
    let options = poll::options(&state.db, poll.id).await?;
    let own_votes = match viewer {
        Some(viewer) => poll::voted_option_positions(&state.db, poll.id, viewer.id).await?,
        None => Vec::new(),
    };
    Ok(Poll::build(poll, &options, &own_votes, Utc::now()))
}

/// What the vote endpoint refuses before it writes anything, checked in
/// the order the ticket pins: the deadline, the voter's own ballot, the
/// author's standing, and the body's choices (missing, out of range, or
/// too many for a single-choice poll).
///
/// The success answer is the [`Uuid`]s of the options the voter marked,
/// deduplicated and in first-requested order — exactly what
/// [`poll::cast_vote`] records.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with each refusal's pinned wording; any
/// database failure arrives through `?` as [`AppError::Database`].
async fn accepted_marks(
    state: &AppState,
    poll: &PollRow,
    posted: &StatusRow,
    voter: &User,
    requested: &[i32],
) -> AppResult<Vec<Uuid>> {
    if poll_closed(poll, Utc::now()) {
        return Err(AppError::Unprocessable(POLL_FINISHED.to_owned()));
    }
    if poll::has_voted(&state.db, poll.id, voter.id).await? {
        return Err(AppError::Unprocessable(ALREADY_VOTED.to_owned()));
    }
    if posted.user_id == voter.id {
        return Err(AppError::Unprocessable(AUTHOR_VOTE.to_owned()));
    }
    if requested.is_empty() {
        return Err(AppError::Unprocessable(MISSING_CHOICES.to_owned()));
    }
    let options = poll::options(&state.db, poll.id).await?;
    chosen_option_ids(poll, &options, requested)
}

/// Whether the poll's deadline has passed at `now`: the same strict
/// comparison the [`Poll`] entity renders as `expired` — a poll at its
/// deadline is still open, a poll with no deadline never closes — so the
/// endpoint's `"Poll's finished"` and the entity's flag can never
/// disagree about the same instant.
fn poll_closed(poll: &PollRow, now: DateTime<Utc>) -> bool {
    poll.expires_at.is_some_and(|deadline| deadline < now)
}

/// The option ids the ballot marks, from the requested `positions`: one
/// per distinct index, in first-requested order (duplicates are ignored,
/// not refused), each index naming a stored option of `poll` by its
/// [`position`](PollOptionRow::position).
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_INDICES`] for an index no
/// option of the poll holds — negative, past the last, or a position the
/// rows skip — checked index by index before the count rule, so an
/// out-of-range second choice on a single-choice poll is the range
/// refusal, as Pleroma's `valid_choice` arm comes before its
/// `count_check`. With [`AppError::Unprocessable`] [`TOO_MANY_CHOICES`]
/// for a second distinct index on a poll that is not
/// [`multiple`](PollRow::multiple).
fn chosen_option_ids(
    poll: &PollRow,
    options: &[PollOptionRow],
    requested: &[i32],
) -> AppResult<Vec<Uuid>> {
    let mut indices: Vec<i32> = Vec::with_capacity(requested.len());
    for index in requested {
        if !indices.contains(index) {
            indices.push(*index);
        }
    }
    let mut chosen: Vec<Uuid> = Vec::with_capacity(indices.len());
    for index in indices {
        let Some(option) = options.iter().find(|option| option.position == index) else {
            return Err(AppError::Unprocessable(INVALID_INDICES.to_owned()));
        };
        chosen.push(option.id);
    }
    if !poll.multiple && chosen.len() > 1 {
        return Err(AppError::Unprocessable(TOO_MANY_CHOICES.to_owned()));
    }
    Ok(chosen)
}

/// The request parameters of `POST /api/v1/polls/{id}/votes`: the
/// `choices` array of option indexes, through [`ApiBody`] (decision 10).
#[derive(Debug, Deserialize)]
struct VoteParams {
    /// The option positions the ballot marks. An absent key, a JSON
    /// `null` and the empty array all deserialize to the empty list,
    /// which the endpoint refuses as [`MISSING_CHOICES`]; a value that
    /// is no integer, in any of the shapes [`flex_choices`] reads, is
    /// the extractor's own 422.
    #[serde(default, deserialize_with = "flex_choices")]
    choices: Vec<i32>,
}

/// Deserializes `choices` from every shape decision 10 admits: the JSON
/// array of integers, the form's repeated `choices[]` keys (strings,
/// which Pleroma's `is_binary` arm coerces the same way), a lone scalar
/// (`choices=0`, the single value Pleroma's `List.wrap` accepts), and
/// the indexed `choices[0]=…` map the form parser leaves as an object —
/// [`index_map_to_vec`] folds each into the list of indexes.
///
/// # Errors
///
/// A serde error — the extractor's 422 — when any element is neither an
/// integer nor an integer's string spelling.
fn flex_choices<'de, D>(deserializer: D) -> Result<Vec<i32>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    let items = value.map_or_else(Vec::new, index_map_to_vec);
    items
        .iter()
        .map(choice_index)
        .collect::<Option<Vec<i32>>>()
        .ok_or_else(|| SerdeError::custom("expected the index of a poll option"))
}

/// The one integer behind a `choices` element: a JSON number within
/// `i32`'s range or a string that parses as one (trimmed), `None` for
/// anything else.
fn choice_index(value: &Value) -> Option<i32> {
    match value {
        Value::Number(number) => number.as_i64().and_then(|int| i32::try_from(int).ok()),
        Value::String(text) => text.trim().parse::<i32>().ok(),
        Value::Bool(_) | Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

/// Post-write hook of `POST /api/v1/polls/{id}/votes`: the tickets that
/// tell the poll's author of a new ballot — Pleroma's `CommonAPI.vote`
/// runs each `Answer` through `Pipeline.common_pipeline`, which lands
/// the notification this server's [`crate::services::notify`] grows —
/// and federate the answers the voter's ballot becomes (decision 6's
/// `ap_id`-bearing activities, queued through the job table, decision
/// 11) wire themselves in here, reading the moved poll from `state`.
///
/// `poll` is the row as the transaction left it, `voter` the account
/// that cast the ballot, and `option_ids` the options it marked — the
/// same ids [`poll::cast_vote`] wrote, which the federation ticket will
/// name as the answers' `inReplyTo` choices.
///
/// Intentionally empty until then, like the reports'
/// [`after_report_created`](crate::api::reports) hook.
const fn after_vote_cast(_state: &AppState, _poll: &PollRow, _voter: &User, _option_ids: &[Uuid]) {}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    /// A [`PollRow`] of the given flags and deadline, for the pure
    /// checks.
    fn poll_row(multiple: bool, expires_at: Option<DateTime<Utc>>) -> PollRow {
        PollRow {
            id: Uuid::now_v7(),
            status_id: Uuid::now_v7(),
            expires_at,
            multiple,
            hide_totals: false,
            voters_count: 0,
            votes_count: 0,
        }
    }

    /// A [`PollOptionRow`] of `poll` at `position`.
    fn option_row(poll_id: Uuid, position: i32) -> PollOptionRow {
        PollOptionRow {
            id: Uuid::now_v7(),
            poll_id,
            position,
            title: format!("option {position}"),
            votes_count: 0,
        }
    }

    /// The instant the deadline tests pivot on: 2026-01-01T12:00:00Z.
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap()
    }

    /// Parses `value` as the vote body's parameters, as the extractor
    /// would.
    fn params_of(value: Value) -> AppResult<VoteParams> {
        crate::extract::into_params(value)
    }

    #[test]
    fn choices_deserialize_from_every_shape_decision_10_admits() {
        for body in [
            json!({"choices": [0, 2]}),
            json!({"choices": ["0", "2"]}),
            json!({"choices": [0, "2"]}),
            json!({"choices": {"0": "0", "1": "2"}}),
        ] {
            let parsed = params_of(body).expect("the shape parses");
            assert_eq!(
                parsed.choices,
                vec![0, 2],
                "this body spells the indexes 0 and 2"
            );
        }
        for lone in [json!({"choices": "0"}), json!({"choices": 0})] {
            let parsed = params_of(lone).expect("a lone value parses");
            assert_eq!(
                parsed.choices,
                vec![0],
                "a single value wraps into the one-element list"
            );
        }
    }

    #[test]
    fn a_missing_null_or_empty_choices_is_the_empty_list() {
        for spelling in ["an absent key", "an empty array", "a null"] {
            let body = match spelling {
                "an absent key" => json!({}),
                "an empty array" => json!({"choices": []}),
                _ => json!({"choices": null}),
            };
            let parsed = params_of(body).expect("the shape parses");
            assert!(parsed.choices.is_empty(), "{spelling} carries no choice");
        }
    }

    #[test]
    fn whitespace_padded_strings_and_negative_integers_parse() {
        let parsed = params_of(json!({"choices": [" 1 ", -2]})).expect("they parse");
        assert_eq!(parsed.choices, vec![1, -2], "both indexes as written");
    }

    #[test]
    fn a_choice_that_is_no_integer_is_the_extractors_422() {
        for body in [
            json!({"choices": ["abc"]}),
            json!({"choices": [true]}),
            json!({"choices": [[0]]}),
            json!({"choices": ["1.5"]}),
        ] {
            let err = params_of(body).expect_err("this body must be refused");
            assert!(
                matches!(err, AppError::Unprocessable(_)),
                "a non-integer choice is the extractor's 422, not a {err}"
            );
        }
    }

    #[test]
    fn duplicates_mark_their_option_once() {
        let stand = poll_row(true, None);
        let options: Vec<PollOptionRow> = (0..3)
            .map(|position| option_row(stand.id, position))
            .collect();
        let chosen = chosen_option_ids(&stand, &options, &[1, 1, 0, 1])
            .expect("a repeated index is ignored, not refused");
        assert_eq!(chosen.len(), 2, "two distinct marks from four values");
        assert_eq!(chosen[0], options[1].id, "in first-requested order");
        assert_eq!(chosen[1], options[0].id);
    }

    #[test]
    fn an_index_no_option_holds_is_invalid() {
        let stand = poll_row(true, None);
        let options: Vec<PollOptionRow> = (0..2)
            .map(|position| option_row(stand.id, position))
            .collect();
        for requested in [vec![2], vec![-1], vec![0, 5], vec![0, -2]] {
            let err = chosen_option_ids(&stand, &options, &requested)
                .expect_err("an out-of-range index is refused");
            assert!(
                matches!(&err, AppError::Unprocessable(message) if message == INVALID_INDICES),
                "`{requested:?}` names no option: {err}"
            );
        }
    }

    #[test]
    fn two_distinct_choices_on_a_single_choice_poll_are_too_many() {
        let stand = poll_row(false, None);
        let options: Vec<PollOptionRow> = (0..2)
            .map(|position| option_row(stand.id, position))
            .collect();
        let err = chosen_option_ids(&stand, &options, &[0, 1])
            .expect_err("a single-choice poll admits one mark");
        assert!(
            matches!(&err, AppError::Unprocessable(message) if message == TOO_MANY_CHOICES),
            "two marks are refused by name: {err}"
        );
        // Duplicates first: after deduplication this is one mark, which
        // a single-choice poll admits.
        assert_eq!(
            chosen_option_ids(&stand, &options, &[1, 1]).expect("duplicates leave one mark"),
            vec![options[1].id],
            "and the ballot marks that one option"
        );
        // And the range rule answers before the count rule: both
        // requested indexes are out of range.
        let range_first =
            chosen_option_ids(&stand, &options, &[3, 4]).expect_err("the range check comes first");
        assert!(
            matches!(&range_first, AppError::Unprocessable(message) if message == INVALID_INDICES),
            "an out-of-range pair is Invalid indices, not Too many: {range_first}"
        );
    }

    #[test]
    fn a_multiple_choice_poll_admits_every_distinct_mark() {
        let stand = poll_row(true, None);
        let options: Vec<PollOptionRow> = (0..3)
            .map(|position| option_row(stand.id, position))
            .collect();
        assert_eq!(
            chosen_option_ids(&stand, &options, &[2, 1, 0, 2])
                .expect("three distinct marks are a ballot"),
            vec![options[2].id, options[1].id, options[0].id],
            "deduplicated, in first-requested order"
        );
    }

    #[test]
    fn the_position_rule_follows_the_stored_positions_not_their_count() {
        // A poll whose options hold positions 1 and 7: the indexes vote
        // by, and the entity's `own_votes` name, are the stored ones.
        let stand = poll_row(false, None);
        let options = vec![option_row(stand.id, 1), option_row(stand.id, 7)];
        assert_eq!(
            chosen_option_ids(&stand, &options, &[7]).expect("a stored position marks"),
            vec![options[1].id]
        );
        let skipped = chosen_option_ids(&stand, &options, &[0])
            .expect_err("a skipped position names no option");
        assert!(
            matches!(&skipped, AppError::Unprocessable(message) if message == INVALID_INDICES),
            "the row's absence is the range refusal: {skipped}"
        );
    }

    #[test]
    fn the_closed_rule_matches_the_entity_s_strict_expiry() {
        let deadline = now();
        assert!(
            !poll_closed(&poll_row(false, None), deadline),
            "a poll with no deadline never closes"
        );
        let with_deadline = poll_row(false, Some(deadline));
        assert!(
            !poll_closed(&with_deadline, deadline),
            "at the deadline itself the poll is still open"
        );
        assert!(
            !poll_closed(&with_deadline, deadline - chrono::TimeDelta::seconds(1)),
            "and a second before it"
        );
        assert!(
            poll_closed(&with_deadline, deadline + chrono::TimeDelta::seconds(1)),
            "one second past it is finished"
        );
    }
}
