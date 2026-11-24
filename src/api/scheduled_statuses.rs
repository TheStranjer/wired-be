//! Scheduled statuses over HTTP (`/api/v1/scheduled_statuses`): the four
//! routes an account has for the posts it parked for later — the listing, a
//! single schedule, moving its fire moment, and cancelling it.
//!
//! The reference is Pleroma's `ScheduledActivityController`
//! (`lib/pleroma/web/mastodon_api/controllers/scheduled_activity_controller.ex`).
//! The rows themselves are [`crate::services::schedule_status`]'s and
//! [`crate::jobs::scheduled_statuses`]' — this module manages schedules, it
//! never posts, and nothing it cancels can post afterwards.
//!
//! - `GET /api/v1/scheduled_statuses` answers the caller's pending schedules
//!   as [`PageParams`] pages (default 20 rows, [`MAX_LIMIT`] at most) of
//!   [`ScheduledStatus`] entities, [`page_for_user`]'s newest-id-first order,
//!   and Pleroma's `add_link_headers` `Link` header over the ids of the page's
//!   first and last row. A schedule has no state between being parked and
//!   being fired or cancelled, so every row of the table is a pending one:
//!   the listing is the account's whole slice of it, paged.
//! - `GET /api/v1/scheduled_statuses/{id}` answers one schedule as the same
//!   entity.
//! - `PUT /api/v1/scheduled_statuses/{id}` moves it. The one parameter is
//!   `scheduled_at` — RFC3339 text, and the ticket's "same minimum-future rule
//!   as creation" is literally the create's own [`check_window`], so a past or
//!   too-soon moment is the same 422 naming the configured minutes. What the
//!   schedule *says* is not up for revision: the parked params stay as the
//!   create normalised them, because when a post fires is the one fact about a
//!   schedule a client may still change. The create's two count limits are not
//!   re-run: a move stores no new row, so it cannot breach a count it already
//!   sits inside. The queued fire follows the row
//!   ([`jobs::scheduled_statuses::reschedule`]: the waiting job deleted by its
//!   `unique_key`, a new one enqueued at the new moment), and the answer is the
//!   moved entity.
//! - `DELETE /api/v1/scheduled_statuses/{id}` cancels: row and waiting job
//!   both go, and the answer is the empty object `{}` Pleroma's `delete`
//!   renders.
//!
//! Every route needs a user token ([`AuthUser`]) and, per Pleroma's
//! `OAuthScopesPlug` pair on this controller, `read:statuses` to look and
//! `write:statuses` to move and cancel. Ids are read as
//! [`parse_id`] reads them and every row through [`get_owned`], so a malformed,
//! unknown or foreign id is the same 404 `{"error":"Record not found"}` —
//! Pleroma's `assign_scheduled_activity` answers all three with one `nil`, and
//! a probe never learns that somebody else's schedule exists.
//!
//! A schedule's `media_attachments` are the only read beyond its own row: the
//! ids its params reserve, in the order the params hold them, exactly as the
//! create answered them, with one page's worth of ids batched into a single
//! read. No route here writes those rows, so cancelling is what releases a
//! reserved attachment; moving a schedule leaves it reserved.
//!
//! [`MAX_LIMIT`]: crate::pagination::MAX_LIMIT
//! [`get_owned`]: crate::models::scheduled_status::get_owned
//! [`page_for_user`]: crate::models::scheduled_status::page_for_user
//! [`jobs::scheduled_statuses::reschedule`]: crate::jobs::scheduled_statuses::reschedule

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
    routing::get,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Map, Value};
use tracing::debug;
use uuid::Uuid;

use crate::{
    auth::AuthUser,
    config::Config,
    entities::scheduled_status::ScheduledStatus,
    error::{AppError, AppResult},
    extract::ApiParams,
    models::{
        job,
        media_attachment::{self, MediaAttachment as AttachmentRow},
        scheduled_status::{self, ScheduledStatus as StoredRow},
    },
    oauth::scopes::{READ_STATUSES, WRITE_STATUSES},
    pagination::{NO_EXTRA_QUERY, PageParams, keyset_link, set_link_header},
    services::schedule_status::check_window,
    state::AppState,
    util::{id::parse_id, time::parse_time},
};

/// 422 body for a `scheduled_at` the move cannot read as an instant, absent
/// included: the create's own wording, so an unreadable moment is refused the
/// same way whichever route was asked.
const INVALID_SCHEDULED_AT: &str = "Invalid scheduled_at";

/// The `/api/v1/scheduled_statuses` router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/scheduled_statuses", get(list_scheduled_statuses))
        .route(
            "/scheduled_statuses/{id}",
            get(show_scheduled_status)
                .put(update_scheduled_status)
                .delete(delete_scheduled_status),
        )
}

/// `GET /api/v1/scheduled_statuses` — the caller's pending schedules, one
/// [`PageParams`] page at a time.
///
/// The `Link` header is [`keyset_link`] over the first and last row's id, and
/// an empty page answers `[]` with no `Link` header at all. Its path is the
/// request's own [`OriginalUri`], not the [`Uri`] the router hands a nested
/// handler: axum strips the `/api/v1` prefix off the latter, and Pleroma links
/// to `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `read:statuses` check; [`AppError::BadRequest`] from [`PageParams`] for
/// a malformed cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value that
/// is not header-safe as [`AppError::Internal`].
///
/// [`Uri`]: axum::http::Uri
async fn list_scheduled_statuses(
    State(state): State<AppState>,
    auth: AuthUser,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<ScheduledStatus>>)> {
    auth.require(READ_STATUSES)?;
    let rows = scheduled_status::page_for_user(&state.db, auth.0.id, page).await?;
    let entities = entities_for(&state, &rows).await?;
    let link = keyset_link(
        &state.config.base_url,
        uri.path(),
        &NO_EXTRA_QUERY,
        &rows,
        |row| row.id,
    );
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    debug!(
        count = entities.len(),
        account = %auth.0.id,
        "scheduled statuses listed"
    );
    Ok((headers, Json(entities)))
}

/// `GET /api/v1/scheduled_statuses/{id}` — one of the caller's schedules as
/// the [`ScheduledStatus`] entity.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `read:statuses` check; [`AppError::NotFound`] when `id` is malformed,
/// unknown or another account's (see [`owned_row`]). Any database failure
/// arrives through `?` as [`AppError::Database`].
async fn show_scheduled_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<ScheduledStatus>> {
    auth.require(READ_STATUSES)?;
    let row = owned_row(&state, &id, auth.0.id).await?;
    Ok(Json(entity(&state, &row).await?))
}

/// `PUT /api/v1/scheduled_statuses/{id}` — move the caller's schedule to the
/// moment `scheduled_at` names and answer it; what the schedule will post
/// stays exactly as the create stored it.
///
/// The three checks run in Pleroma's plug order: the body first (its
/// `CastAndValidate`, so unreadable text never reaches a lookup), ownership
/// second (its `assign_scheduled_activity`, so an unknown id is a 404 rather
/// than a lesson in the window), and the window third, before anything is
/// written. The queue then follows the row.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:statuses` check; [`AppError::Unprocessable`] with
/// [`INVALID_SCHEDULED_AT`] for a `scheduled_at` that is absent or no instant,
/// and with the window's own wording for a moment too near (or past);
/// [`AppError::NotFound`] when `id` is malformed, unknown or another
/// account's. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn update_scheduled_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiParams(params): ApiParams<RescheduleParams>,
) -> AppResult<Json<ScheduledStatus>> {
    auth.require(WRITE_STATUSES)?;
    let at = requested_moment(params.scheduled_at.as_deref())?;
    let row = owned_row(&state, &id, auth.0.id).await?;
    check_window(&state.config, at)?;
    let Some(moved) = scheduled_status::reschedule(&state.db, row.id, auth.0.id, at).await? else {
        // Reachable only when the schedule fired or was cancelled between the
        // read above and this write: the same 404 a foreign id gets, since
        // there is nothing left here to move.
        return Err(AppError::NotFound);
    };
    crate::jobs::scheduled_statuses::reschedule(&state, &moved).await;
    debug!(
        scheduled = %moved.id,
        account = %auth.0.id,
        due = %moved.scheduled_at,
        "scheduled status moved"
    );
    Ok(Json(entity(&state, &moved).await?))
}

/// `DELETE /api/v1/scheduled_statuses/{id}` — cancel one of the caller's
/// schedules: the row goes, the fire waiting for it goes with it, and the
/// answer is `{}`.
///
/// The job is taken by the same `unique_key` [`schedule`] queued it with, so
/// one cancel empties a schedule's whole queue presence. A fire a worker had
/// already claimed cannot be taken back; it reads the row again and finds
/// nothing, so a cancelled schedule posts nothing either way (see
/// [`crate::jobs::scheduled_statuses::handle_publish`]).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:statuses` check; [`AppError::NotFound`] when `id` is malformed,
/// unknown or another account's (see [`owned_row`]). Any database failure
/// arrives through `?` as [`AppError::Database`].
///
/// [`schedule`]: crate::jobs::scheduled_statuses::schedule
async fn delete_scheduled_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_STATUSES)?;
    let row = owned_row(&state, &id, auth.0.id).await?;
    if !scheduled_status::delete_owned(&state.db, row.id, auth.0.id).await? {
        return Err(AppError::NotFound);
    }
    let key = crate::jobs::scheduled_statuses::unique_key(row.id);
    let cancelled = job::cancel_pending(&state.db, &key).await?;
    debug!(
        scheduled = %row.id,
        account = %auth.0.id,
        cancelled,
        "scheduled status cancelled"
    );
    Ok(Json(Value::Object(Map::new())))
}

/// The request parameters of `PUT /api/v1/scheduled_statuses/{id}`: the one
/// field of Pleroma's update schema, optional at the serde layer so that an
/// absent `scheduled_at` is the endpoint's 422 rather than a failed
/// deserialization.
#[derive(Debug, Deserialize)]
struct RescheduleParams {
    /// The new fire moment as RFC3339 text, read by [`parse_time`].
    #[serde(default)]
    scheduled_at: Option<String>,
}

/// The instant the `scheduled_at` parameter names, read the way the create
/// reads its own; the window is the next check, not this one's, because
/// Pleroma validates the shape before it has even found the row.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_SCHEDULED_AT`] when the key is
/// absent, `null`, or text that is no RFC3339 instant.
fn requested_moment(text: Option<&str>) -> AppResult<DateTime<Utc>> {
    text.and_then(parse_time)
        .ok_or_else(|| AppError::Unprocessable(INVALID_SCHEDULED_AT.to_owned()))
}

/// The schedule `id` names, if `user_id` owns it — the one read the three
/// id-keyed routes share, which is why they cannot tell a stranger's schedule
/// from a missing one.
///
/// # Errors
///
/// [`AppError::NotFound`] for id text that is no id and for a row this account
/// does not have, one answer for both; any database failure arrives as
/// [`AppError::Database`].
async fn owned_row(state: &AppState, id: &str, user_id: Uuid) -> AppResult<StoredRow> {
    let Some(schedule_id) = parse_id(id) else {
        return Err(AppError::NotFound);
    };
    scheduled_status::get_owned(&state.db, schedule_id, user_id)
        .await?
        .ok_or(AppError::NotFound)
}

/// The [`ScheduledStatus`] entities for `rows`, their reserved attachments
/// read once for the whole page.
///
/// # Errors
///
/// Any database failure behind the attachment batch arrives as
/// [`AppError::Database`].
async fn entities_for(state: &AppState, rows: &[StoredRow]) -> AppResult<Vec<ScheduledStatus>> {
    let asked: Vec<Uuid> = rows.iter().flat_map(media_ids).collect();
    let by_id = media_by_id(state, &asked).await?;
    Ok(rows
        .iter()
        .map(|row| render(&state.config, row, &by_id))
        .collect())
}

/// One schedule as its entity, its reserved attachments read for it alone.
///
/// # Errors
///
/// Any database failure behind the attachment read arrives as
/// [`AppError::Database`].
async fn entity(state: &AppState, row: &StoredRow) -> AppResult<ScheduledStatus> {
    let by_id = media_by_id(state, &media_ids(row)).await?;
    Ok(render(&state.config, row, &by_id))
}

/// [`ScheduledStatus::from_row`] for `row` against the batched rows `by_id`
/// holds: the attachments its params name, in that submission order, an id
/// whose row has since been deleted contributing nothing.
fn render(
    config: &Config,
    row: &StoredRow,
    by_id: &HashMap<Uuid, AttachmentRow>,
) -> ScheduledStatus {
    let media: Vec<AttachmentRow> = media_ids(row)
        .iter()
        .filter_map(|id| by_id.get(id).cloned())
        .collect();
    ScheduledStatus::from_row(row, &media, config)
}

/// The attachment ids `row`'s stored params reserve, in submission order.
/// Params with no `media_ids` array — or one holding no id text — reserve
/// nothing.
fn media_ids(row: &StoredRow) -> Vec<Uuid> {
    row.params
        .get("media_ids")
        .and_then(Value::as_array)
        .map_or_default(|ids| {
            ids.iter()
                .filter_map(|id| id.as_str().and_then(parse_id))
                .collect()
        })
}

/// The attachment rows behind `ids`, keyed by id; asking about nothing reads
/// nothing.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
async fn media_by_id(state: &AppState, ids: &[Uuid]) -> AppResult<HashMap<Uuid, AttachmentRow>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = media_attachment::get_many(&state.db, ids).await?;
    Ok(rows.into_iter().map(|found| (found.id, found)).collect())
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            media_attachment::NewMediaAttachment,
            user::{NewUser, User, insert as insert_user, local_ap_id},
        },
        util::time::mastodon_time,
    };

    /// The instant every fixture row is due at and stamped with, on the
    /// wire's own millisecond precision.
    const DUE: &str = "2026-05-18T09:00:00.000Z";
    /// The hyphenated id of the fixture row [`stored_row`] builds.
    const ROW_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";

    /// The whole instant [`DUE`] names, in UTC.
    fn due() -> DateTime<Utc> {
        parse_time(DUE).expect("the fixture instant parses")
    }

    /// One stored schedule parking `params`, due at [`DUE`].
    fn stored_row(params: Value) -> StoredRow {
        StoredRow {
            id: Uuid::parse_str(ROW_ID).expect("the fixture id parses"),
            user_id: Uuid::now_v7(),
            scheduled_at: due(),
            params,
            inserted_at: due(),
            updated_at: due(),
        }
    }

    /// The rows of a page in the order [`keyset_link`] receives them: `ids`
    /// verbatim, newest id first.
    fn stored_page(ids: &[&str]) -> Vec<StoredRow> {
        ids.iter()
            .map(|id| StoredRow {
                id: Uuid::parse_str(id).expect("the fixture ids are ids"),
                user_id: Uuid::now_v7(),
                scheduled_at: due(),
                params: json!({ "status": "x" }),
                inserted_at: due(),
                updated_at: due(),
            })
            .collect()
    }

    /// Test state over `pool` with the fixed test config.
    fn app_state(pool: &PgPool) -> AppState {
        AppState::for_tests(pool.clone(), Config::for_tests())
    }

    /// A local user named `nickname`, to own fixture uploads.
    async fn local_user(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// An unposted `image/png` upload of `owner`, named for its url.
    async fn upload(pool: &PgPool, owner: &User, name: &str) -> AttachmentRow {
        media_attachment::insert(
            pool,
            &NewMediaAttachment {
                user_id: Some(owner.id),
                url: format!("{TEST_BASE_URL}/media/{name}.png"),
                content_type: "image/png".to_owned(),
                ..NewMediaAttachment::default()
            },
        )
        .await
        .expect("a fixture upload inserts")
    }

    /// A stored attachment row for `id`, bare enough to render.
    fn attachment(id: Uuid) -> AttachmentRow {
        AttachmentRow {
            id,
            user_id: Some(Uuid::now_v7()),
            status_id: None,
            position: 0,
            storage_key: Some("media/x.png".to_owned()),
            remote_url: None,
            url: format!("{TEST_BASE_URL}/media/x.png"),
            preview_url: None,
            content_type: "image/png".to_owned(),
            description: None,
            blurhash: None,
            width: None,
            height: None,
            preview_width: None,
            preview_height: None,
            duration: None,
            focus_x: None,
            focus_y: None,
            file_size: None,
            sensitive_hint: false,
            inserted_at: due(),
            updated_at: due(),
        }
    }

    #[test]
    fn the_move_needs_one_readable_instant() {
        let at = requested_moment(Some(DUE)).expect("an RFC3339 moment reads");
        assert_eq!(at, due(), "the instant goes through as read");
        assert_eq!(
            requested_moment(Some("2026-05-18T11:00:00+02:00")).expect("an offset reads too"),
            at,
            "an offset spelling normalises to the same UTC instant"
        );

        for text in [None, Some("not-a-date"), Some(""), Some("1779000000")] {
            let err = requested_moment(text).expect_err(&format!("`{text:?}` is no instant"));
            assert_eq!(err.to_string(), INVALID_SCHEDULED_AT, "one wording: {err}");
        }
    }

    #[test]
    fn a_moment_moves_only_inside_the_creates_window() {
        let config = Config::for_tests();
        let err = check_window(&config, Utc::now() - TimeDelta::hours(1))
            .expect_err("an hour ago is not in the future");
        assert_eq!(
            err.to_string(),
            "Scheduled time must be at least 5 minutes in the future",
            "the create's refusal, word for word, for a move too"
        );
        check_window(&config, Utc::now() + TimeDelta::hours(1))
            .expect("an hour ahead is inside no window");
    }

    #[test]
    fn media_ids_keeps_the_params_order_and_nothing_else() {
        let first = Uuid::now_v7().to_string();
        let second = Uuid::now_v7().to_string();
        let with_ids = stored_row(json!({
            "status": "pics",
            "media_ids": [first, second, "not-an-id", 7],
        }));
        assert_eq!(
            media_ids(&with_ids),
            vec![
                Uuid::parse_str(&first).expect("the fixture id parses"),
                Uuid::parse_str(&second).expect("the fixture id parses"),
            ],
            "submission order, and the entries that are no id text drop out"
        );

        for params in [
            json!({ "status": "no media key" }),
            json!({ "media_ids": null }),
            json!({ "media_ids": [] }),
            json!({ "media_ids": "not-an-array" }),
        ] {
            assert!(
                media_ids(&stored_row(params.clone())).is_empty(),
                "`{params}` reserves nothing"
            );
        }
    }

    #[test]
    fn render_answers_the_reserved_attachments_in_that_order() {
        let older = Uuid::now_v7();
        let newer = Uuid::now_v7();
        let gone = Uuid::now_v7();
        let one = stored_row(json!({
            "status": "pics",
            "media_ids": [newer.to_string(), older.to_string(), gone.to_string()],
        }));
        let by_id: HashMap<Uuid, AttachmentRow> = [older, newer]
            .iter()
            .map(|id| (*id, attachment(*id)))
            .collect();

        let rendered = serde_json::to_value(render(&Config::for_tests(), &one, &by_id))
            .expect("it serializes");
        let attachments = rendered["media_attachments"]
            .as_array()
            .expect("media_attachments is an array");
        assert_eq!(
            attachments.len(),
            2,
            "the two stored rows answer; the deleted id contributes nothing"
        );
        assert_eq!(
            attachments[0]["id"],
            newer.to_string(),
            "the params' first id answers first"
        );
        assert_eq!(attachments[1]["id"], older.to_string(), "then the second");
        assert_eq!(
            attachments[0]["url"],
            format!("{TEST_BASE_URL}/media/x.png"),
            "each entry is the full Attachment entity"
        );
        assert_eq!(
            rendered["params"]["media_ids"],
            json!([newer.to_string(), older.to_string(), gone.to_string()]),
            "the params echo every id, read or not"
        );
        assert_eq!(rendered["id"], ROW_ID, "the entity is the row's own id");
        assert_eq!(
            rendered["scheduled_at"], DUE,
            "and the row's moment in the Mastodon format"
        );
    }

    #[test]
    fn the_link_header_walks_from_both_ends_of_the_page() {
        let newest = "01934567-89ab-7cde-f012-3456789abce0";
        let middle = "01934567-89ab-7cde-f012-3456789abcdf";
        let oldest = "01934567-89ab-7cde-f012-3456789abcde";
        let path = "/api/v1/scheduled_statuses";
        let link = keyset_link(
            TEST_BASE_URL,
            path,
            &NO_EXTRA_QUERY,
            &stored_page(&[newest, middle, oldest]),
            |row| row.id,
        )
        .expect("a full page links");
        assert_eq!(
            link,
            format!(
                "<{TEST_BASE_URL}{path}?max_id={oldest}>; rel=\"next\", \
                 <{TEST_BASE_URL}{path}?min_id={newest}>; rel=\"prev\""
            ),
            "next walks to the oldest row, prev back to the newest, and the middle row is no cursor"
        );
    }

    #[test]
    fn an_empty_page_links_nowhere() {
        assert_eq!(
            keyset_link(
                TEST_BASE_URL,
                "/api/v1/scheduled_statuses",
                &NO_EXTRA_QUERY,
                &stored_page(&[]),
                |row| row.id
            ),
            None,
            "no rows, no Link header"
        );
        let only = "01934567-89ab-7cde-f012-3456789abcde";
        let link = keyset_link(
            TEST_BASE_URL,
            "/p",
            &NO_EXTRA_QUERY,
            &stored_page(&[only]),
            |row| row.id,
        )
        .expect("a one-row page links");
        assert!(
            link.contains(&format!("max_id={only}")) && link.contains(&format!("min_id={only}")),
            "and its single id is both cursors: {link}"
        );
    }

    #[tokio::test]
    async fn an_empty_ask_reads_no_attachments() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = app_state(db.pool());
        let alice = local_user(&state.db, "alice").await;
        let kept = upload(&state.db, &alice, "kept").await;

        assert!(
            media_by_id(&state, &[])
                .await
                .expect("the empty ask runs")
                .is_empty(),
            "asking about nothing reads nothing and answers the empty map"
        );

        let by_id = media_by_id(&state, &[kept.id, Uuid::now_v7()])
            .await
            .expect("the batch read runs");
        assert_eq!(by_id.len(), 1, "the stored row, the unknown id absent");
        assert!(
            by_id.contains_key(&kept.id),
            "and the row is keyed by its own id"
        );
    }

    #[tokio::test]
    async fn a_page_of_rows_is_one_batch_and_keeps_its_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = app_state(db.pool());
        let alice = local_user(&state.db, "alice").await;
        let first = upload(&state.db, &alice, "first").await;
        let second = upload(&state.db, &alice, "second").await;

        // Two schedules sharing one attachment, each naming it in a different
        // position: one page of entities, and every entity keeps its own row's
        // order.
        let rows = vec![
            stored_row(json!({ "status": "a", "media_ids": [second.id.to_string()] })),
            stored_row(json!({
                "status": "b",
                "media_ids": [first.id.to_string(), second.id.to_string()],
            })),
        ];
        let entities = entities_for(&state, &rows).await.expect("the page renders");
        assert_eq!(entities.len(), 2, "one entity per row, in the page's order");
        let rendered = serde_json::to_value(&entities).expect("the entities serialize");
        let short = rendered[0]["media_attachments"]
            .as_array()
            .expect("media_attachments is an array");
        assert_eq!(short.len(), 1, "the first schedule answers its one id");
        assert_eq!(short[0]["id"], second.id.to_string(), "the id it named");
        let long = rendered[1]["media_attachments"]
            .as_array()
            .expect("media_attachments is an array");
        assert_eq!(long.len(), 2, "the second answers both");
        assert_eq!(
            long[0]["id"],
            first.id.to_string(),
            "in its own submission order, not the id order"
        );
        assert_eq!(long[1]["id"], second.id.to_string());

        let alone = serde_json::to_value(entity(&state, &rows[1]).await.expect("one row renders"))
            .expect("the entity serializes");
        assert_eq!(
            alone["media_attachments"], rendered[1]["media_attachments"],
            "and a row read alone answers exactly what the batch read answered"
        );
    }

    #[tokio::test]
    async fn a_schedule_that_reserves_nothing_needs_no_read() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = app_state(db.pool());

        let rendered = serde_json::to_value(
            entity(&state, &stored_row(json!({ "status": "no attachments" })))
                .await
                .expect("the entity renders"),
        )
        .expect("the entity serializes");
        assert_eq!(
            rendered["media_attachments"],
            json!([]),
            "params that reserve nothing answer no attachments"
        );
        assert_eq!(rendered["params"]["media_ids"], json!([]), "and no ids");
        assert_eq!(
            rendered["scheduled_at"],
            mastodon_time(&due()),
            "the row's own moment leads the entity"
        );
    }
}
