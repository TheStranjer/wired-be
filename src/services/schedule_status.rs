//! The `scheduled_at` arm of `POST /api/v1/statuses`: park the whole create
//! until a future moment instead of posting it now.
//!
//! The reference is Pleroma's `StatusController.create` `scheduled_at` arm
//! (`lib/pleroma/web/mastodon_api/controllers/status_controller.ex`), which
//! hands the params to `Pleroma.Schedule` and answers a `ScheduledActivity`
//! instead of a status: nothing is posted, the request is stored, and
//! `ScheduledActivityWorker`
//! (`lib/pleroma/workers/scheduled_activity_worker.ex`) replays it at its
//! moment. Here the storage is [`crate::models::scheduled_status`], the
//! replay is the [`scheduled_statuses.publish`](crate::jobs::scheduled_statuses::KIND_PUBLISH)
//! job, and this module is the decide-what-to-park half the endpoint runs
//! in between.
//!
//! The body is decided with the create's own rules, never a second set:
//! [`check_body`](crate::services::create_status::check_body) under the
//! author's `default_scope` and
//! `sensitive_default`, and the same parent/conversation resolution
//! [`create_status`](crate::services::create_status::create_status) runs
//! (the shared `prepared_new_status` step), so an empty text, an unknown
//! `visibility`, a non-positive `expires_in` or an invisible or unknown
//! `in_reply_to_id` are on this arm exactly the 422s and the 404 they are
//! on a plain post. The render that step performs decides nothing that is
//! stored here — the publish renders again at fire time — it only costs a
//! schedule nothing it would not have paid as a post.
//!
//! Three decisions are the schedule's own, in Pleroma's shape. The
//! **window**: `scheduled_at` must sit at least
//! [`Config::min_scheduled_secs`](crate::config::Config::min_scheduled_secs)
//! seconds ahead of now, so any past instant fails at the same gate as a
//! too-soon one, and the refusal names the configured minutes. The
//! **limits**: at most
//! [`Config::max_scheduled_per_user`](crate::config::Config::max_scheduled_per_user)
//! pending rows per account and at most
//! [`Config::max_scheduled_per_user_per_day`](crate::config::Config::max_scheduled_per_user_per_day)
//! within the UTC day the fire moment falls in, both 422
//! `Too many scheduled statuses`. The **media**: every attachment id must
//! be the poster's own upload no status has claimed and no pending
//! schedule already reserves — either failure is the create's own 422
//! [`MEDIA_NOT_FOUND`]. Nothing is written to `media_attachments`: the
//! upload is reserved purely by living in this pending row's params, and
//! the publish attaches it when it fires.
//!
//! What is stored is the whole request, normalised: [`CreateStatusParams`]
//! serialised, with the endpoint's own `preview` and `scheduled_at` keys
//! removed (the fire moment lives in the row's column, and a replay is
//! never a preview), the body's fields replaced by what the create's
//! checks resolved — trimmed text, final visibility and its flags, resolved
//! language, deduplicated id strings, the parent id a reblog folded to —
//! and, when the request had them, the `idempotency` and `application_id`
//! keys the [`ScheduledStatus`] entity echoes. Those extra keys are inert
//! to the publish, which reads
//! the params back as a [`CreateStatusParams`] and ignores what it is not.
//! A scheduled create claims no replay key itself: the row's own claim
//! belongs to the publish at run time.
//!
//! [`MEDIA_NOT_FOUND`]: crate::models::media_attachment::MEDIA_NOT_FOUND

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use tracing::debug;
use uuid::Uuid;

use crate::{
    config::Config,
    entities::scheduled_status::ScheduledStatus,
    error::{AppError, AppResult},
    models::{
        media_attachment::{self, MEDIA_NOT_FOUND, MediaAttachment as AttachmentRow},
        oauth_app::OauthApp,
        scheduled_status::{self, NewScheduledStatus, ScheduledStatus as StoredRow},
        status::NewStatus,
        user::User,
    },
    services::create_status::{CreateStatusParams, prepared_new_status},
    state::AppState,
    util::time::parse_time,
};

/// 422 body for a `scheduled_at` the schedule cannot even read.
const INVALID_SCHEDULED_AT: &str = "Invalid scheduled_at";

/// 422 body for an account at either of its schedule limits (Pleroma's
/// `:schedule_activities_limit_reached` / per-day arm, one wording).
const TOO_MANY_SCHEDULED: &str = "Too many scheduled statuses";

/// Seconds in one minute, for the window refusal's wording.
const SECONDS_PER_MINUTE: usize = 60;

/// Parks the create `params` describes until the moment its `scheduled_at`
/// names, and answers the stored schedule as the [`ScheduledStatus`]
/// entity beside its row.
///
/// Nothing is written before every gate below has passed, and what is
/// written is only the `scheduled_statuses` row: no `statuses` row, no
/// `media_attachments` change, no `after_status_created` hook, no
/// expiration job. `idempotency` — the request's `Idempotency-Key`, when
/// it had one — is stored into the params purely for the entity's echo.
/// The entity's `media_attachments` come back empty here: loading the
/// reserved rows in the order the request submitted them is the
/// endpoint's read.
///
/// The gates run in this order: the [`scheduled_at`] read, the create
/// path's own body checks and parent/conversation resolution (the shared
/// `prepared_new_status` step), the window, the two limits, and the media
/// reservation — so a body that would not post is refused with the post's
/// answer before the schedule says anything of its own.
///
/// The fire is queued through
/// [`jobs::scheduled_statuses::schedule`](crate::jobs::scheduled_statuses::schedule),
/// which already swallows and logs its own failure: the row is stored and
/// the periodic sweep publishes it whatever the enqueue answers.
///
/// [`scheduled_at`]: CreateStatusParams::scheduled_at
///
/// # Errors
///
/// [`AppError::Unprocessable`] for `INVALID_SCHEDULED_AT`, the window
/// refusal, a limit refusal, [`MEDIA_NOT_FOUND`], and every refusal
/// [`check_body`](crate::services::create_status::check_body) owes the
/// body; [`AppError::NotFound`] for the shared parent/conversation 404;
/// [`AppError::Internal`] for params that do not serialise (unreachable
/// for a plain request struct); any database failure arrives through `?`
/// as [`AppError::Database`].
pub async fn schedule_status(
    state: &AppState,
    user: &User,
    app: Option<&OauthApp>,
    idempotency: Option<&str>,
    params: CreateStatusParams,
) -> AppResult<(ScheduledStatus, StoredRow)> {
    let at = parse_schedule(&params)?;
    let new = prepared_new_status(state, user, app, &params).await?;
    check_window(&state.config, at)?;
    check_limits(state, user.id, at).await?;
    check_media(state, user.id, &new.media_ids).await?;
    let stored = stored_params(&params, &new, idempotency, app)?;
    let row = scheduled_status::insert(
        &state.db,
        &NewScheduledStatus {
            user_id: user.id,
            scheduled_at: at,
            params: stored,
        },
    )
    .await?;
    crate::jobs::scheduled_statuses::schedule(state, &row).await;
    debug!(
        scheduled = %row.id,
        user = %user.id,
        due = %row.scheduled_at,
        "status scheduled"
    );
    Ok((ScheduledStatus::from_row(&row, &[], &state.config), row))
}

/// The fire moment the body names, read as [`parse_time`] reads RFC3339
/// text; the caller has already established the key is present, so
/// anything that does not parse — including the present-but-unreadable —
/// is the schedule's own 422, ahead of every other decision.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_SCHEDULED_AT`].
fn parse_schedule(params: &CreateStatusParams) -> AppResult<DateTime<Utc>> {
    params
        .scheduled_at
        .as_deref()
        .and_then(parse_time)
        .ok_or_else(|| AppError::Unprocessable(INVALID_SCHEDULED_AT.to_owned()))
}

/// The window: `at` must be at least
/// [`Config::min_scheduled_secs`](crate::config::Config::min_scheduled_secs)
/// seconds ahead of now, so any past instant fails this same gate. The
/// refusal names the setting in whole minutes (the default `300` reads "5
/// minutes"); the comparison truncates the span to whole seconds, which at
/// worst lets a sub-second boundary through and refuses nothing it should
/// accept.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `Scheduled time must be at least
/// {minutes} minutes in the future`.
fn check_window(config: &Config, at: DateTime<Utc>) -> AppResult<()> {
    let floor_secs = i64::try_from(config.min_scheduled_secs).unwrap_or(i64::MAX);
    if at.signed_duration_since(Utc::now()).num_seconds() >= floor_secs {
        return Ok(());
    }
    let minutes = config
        .min_scheduled_secs
        .checked_div(SECONDS_PER_MINUTE)
        .unwrap_or(config.min_scheduled_secs);
    Err(AppError::Unprocessable(format!(
        "Scheduled time must be at least {minutes} minutes in the future"
    )))
}

/// The two counts a schedule may not breach: the account's pending total
/// against [`Config::max_scheduled_per_user`](crate::config::Config::max_scheduled_per_user),
/// then the rows due within the UTC day [`utc_day_bounds`] draws around
/// `at` against its per-day cap. Every row of the table is pending — the
/// fire and the cancel both delete — so the counts are plain reads, and
/// this row's own insert is what the counts refuse ahead of.
///
/// [`utc_day_bounds`]: scheduled_status::utc_day_bounds
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`TOO_MANY_SCHEDULED`] for either
/// breach.
async fn check_limits(state: &AppState, user_id: Uuid, at: DateTime<Utc>) -> AppResult<()> {
    let pending = scheduled_status::count_pending_for_user(&state.db, user_id).await?;
    if breaches(pending, state.config.max_scheduled_per_user) {
        return Err(AppError::Unprocessable(TOO_MANY_SCHEDULED.to_owned()));
    }
    let (day_start, day_end) = scheduled_status::utc_day_bounds(at);
    let in_day =
        scheduled_status::count_pending_for_user_between(&state.db, user_id, day_start, day_end)
            .await?;
    if breaches(in_day, state.config.max_scheduled_per_user_per_day) {
        return Err(AppError::Unprocessable(TOO_MANY_SCHEDULED.to_owned()));
    }
    Ok(())
}

/// Whether `count` schedules already reached `limit`: the database's `i64`
/// read as the config's `usize` — a `count(*)` is never negative, and a
/// count too wide to fit the comparison width at all breaches rather than
/// slipping through.
fn breaches(count: i64, limit: usize) -> bool {
    usize::try_from(count).unwrap_or(usize::MAX) >= limit
}

/// The media gate: every id the checked body names must be a row that
/// exists, is this account's own upload, and is claimed by no status —
/// and no pending schedule (anybody's) may already name it in its stored
/// `media_ids`. Any failure is the create's own [`MEDIA_NOT_FOUND`] 422,
/// which never says which of the four the id was. Nothing is written:
/// the reservation is the pending row's params, read back by
/// [`reserved_media_ids`], and the publish performs the actual attach
/// through the ordinary create.
///
/// [`reserved_media_ids`]: scheduled_status::reserved_media_ids
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`MEDIA_NOT_FOUND`], plus any
/// database failure as [`AppError::Database`].
async fn check_media(state: &AppState, user_id: Uuid, ids: &[Uuid]) -> AppResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let rows = media_attachment::get_many(&state.db, ids).await?;
    let by_id: HashMap<Uuid, &AttachmentRow> = rows.iter().map(|row| (row.id, row)).collect();
    if !ids
        .iter()
        .all(|id| claimable(by_id.get(id).copied(), user_id))
    {
        return Err(media_refusal());
    }
    let reserved = scheduled_status::reserved_media_ids(&state.db, ids).await?;
    if ids.iter().any(|id| reserved.contains(id)) {
        return Err(media_refusal());
    }
    Ok(())
}

/// Whether `row` — the stored attachment behind one submitted id, if any
/// — is this poster's upload that no status has taken yet.
fn claimable(row: Option<&AttachmentRow>, user_id: Uuid) -> bool {
    row.is_some_and(|found| found.user_id == Some(user_id) && found.status_id.is_none())
}

/// The media refusal, shared by both arms of [`check_media`].
fn media_refusal() -> AppError {
    AppError::Unprocessable(MEDIA_NOT_FOUND.to_owned())
}

/// The jsonb the row parks: the request struct as the
/// [`publish`](crate::jobs::scheduled_statuses::handle_publish) will read
/// it back, normalised the way the create resolved it. `preview` and
/// `scheduled_at` are dropped (neither is a replay input, and the fire
/// moment is the row's column); everything the body left to a default or
/// a resolution is written back from `new`; and `idempotency` and
/// `application_id` join when the request had them, as the entity's echo
/// keys the replay ignores.
///
/// # Errors
///
/// [`AppError::Internal`] for params that serialise to no object
/// (unreachable: [`CreateStatusParams`] is a plain struct).
fn stored_params(
    params: &CreateStatusParams,
    new: &NewStatus,
    idempotency: Option<&str>,
    app: Option<&OauthApp>,
) -> AppResult<Value> {
    let serialized = serde_json::to_value(params).map_err(|err| {
        AppError::Internal(format!("the create-status params do not write: {err}"))
    })?;
    let Value::Object(mut map) = serialized else {
        return Err(AppError::Internal(
            "the create-status params wrote no object".to_owned(),
        ));
    };
    let _no_preview: Option<Value> = map.remove("preview");
    let _no_fire_moment: Option<Value> = map.remove("scheduled_at");
    write_resolved(&mut map, new);
    if let Some(key) = idempotency {
        put(&mut map, "idempotency", Value::String(key.to_owned()));
    }
    if let Some(found) = app {
        put(
            &mut map,
            "application_id",
            Value::String(found.id.to_string()),
        );
    }
    Ok(Value::Object(map))
}

/// The keys [`stored_params`] restates from the create's own resolutions:
/// the trimmed text, the final warning, flag and audience (the author's
/// defaults, the parent's cap and the conversation's forcing all decided
/// by [`prepared_new_status`] already), the resolved language and MIME
/// type, the deduplicated attachment ids in submission order, and the
/// parent id — a reblog's id already folded to the original the replay
/// will answer. The expiry is not restated: `expires_in` is resolved
/// neither here nor at insert, and the serialised body value — the whole
/// seconds a later fire counts out from its own moment — rides along.
fn write_resolved(map: &mut Map<String, Value>, new: &NewStatus) {
    put(map, "status", Value::String(new.raw_text.clone()));
    put(map, "spoiler_text", Value::String(new.spoiler_text.clone()));
    put(map, "sensitive", Value::Bool(new.sensitive));
    put(
        map,
        "visibility",
        Value::String(new.visibility.as_str().to_owned()),
    );
    put(
        map,
        "language",
        new.language.clone().map_or(Value::Null, Value::String),
    );
    put(map, "content_type", Value::String(new.content_type.clone()));
    put(
        map,
        "media_ids",
        Value::Array(
            new.media_ids
                .iter()
                .map(|id| Value::String(id.to_string()))
                .collect(),
        ),
    );
    put(
        map,
        "in_reply_to_id",
        new.in_reply_to_id
            .map_or(Value::Null, |id| Value::String(id.to_string())),
    );
}

/// Sets `key` to `value`, dropping whatever the submitted body had there;
/// the drop is the point of the borrow, not a result to keep.
fn put(map: &mut Map<String, Value>, key: &str, value: Value) {
    let _replaced: Option<Value> = map.insert(key.to_owned(), value);
}
