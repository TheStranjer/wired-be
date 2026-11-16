//! The admin statuses surface: reading, re-scoping and deleting posts,
//! and the two narrower listings that walk one account's or one domain's
//! share of them, under `/api/v1/pleroma/admin` (WBE-201).
//!
//! The reference is Pleroma's `AdminAPI.StatusController`
//! (`lib/pleroma/web/admin_api/controllers/status_controller.ex`) beside
//! `AdminAPIController.list_user_statuses`
//! (`lib/pleroma/web/admin_api/controllers/admin_api_controller.ex`) and
//! `AdminAPI.InstanceController.list_statuses`
//! (`lib/pleroma/web/admin_api/controllers/instance_controller.ex`), all
//! three routed in `lib/pleroma/web/router.ex` under the `admin_api`
//! pipeline and its `OAuthScopesPlug` grants `admin:read:statuses` on the
//! reads and `admin:write:statuses` on the two writes. That pipeline's role
//! plug is `UserIsStaffPlug`, which is why every route here takes the
//! [`StaffUser`] extractor — a moderator is Pleroma's expected caller of
//! this controller — and why the scope half of the gate is the
//! [`StaffUser::require`] call each handler makes first, on the area
//! `statuses`.
//!
//! The entities are the ordinary client-API [`Status`], the way Pleroma's
//! `AdminAPI.StatusView` is itself a thin `defdelegate` over
//! `MastodonAPI.StatusView`: no admin-only key is added, and the `account`
//! beside a post is the plain [`Account`](crate::entities::account::Account)
//! entity. What differs is the *view*: every render runs through
//! [`render_statuses`] / [`render_status`] under
//! [`ViewerMode::Admin`], the bypass of the visibility rules the admin
//! surface runs by — see that type for exactly what it lifts.
//!
//! - `GET /statuses` (`index`) — the whole network's ledger, newest post
//!   first, one `page`/`page_size` window (from 1; 50 rows, capped at the
//!   admin listings' 200) of [`status::admin::list`]'s rows answered
//!   as a bare array. The three filters are Pleroma's: `godmode` off keeps
//!   the two public visibilities and on admits `private`, `direct` and
//!   Pleroma's `local` beside them; `local_only` is this instance's own
//!   authors only; `with_reblogs` decides whether boost rows are listed at
//!   all (Pleroma's `exclude_reblogs: not with_reblogs`). There is no
//!   `X-Total-Count`: Pleroma sends none here.
//! - `GET /statuses/{id}` (`show`) — that one post, whatever its
//!   visibility, as [`status::get_by_id`] answers it; a malformed id and
//!   an unknown or tombstoned one are the same 404 `{"error":"Not found"}`
//!   Pleroma's `AdminAPI.FallbackController` gives a miss.
//! - `PUT /statuses/{id}` (`update`) — Pleroma's `update_activity_scope`:
//!   the body's `sensitive` and `visibility` are the two fields the write
//!   moves, each only where the request presented it ([`status::admin::set_scope`]),
//!   and nothing else — no revision is recorded and `edited_at` does not
//!   move, because a scope change is not a change of what was said. A
//!   `visibility` the column would not hold is the 422
//!   `{"error":"Invalid visibility"}`. The write leaves a `status_update`
//!   [`moderation_log`] entry naming the parts that moved, answers the
//!   re-scoped [`Status`] entity, and — for a status of this instance —
//!   runs the edit route's own `after_status_edited` hook
//!   ([`crate::api::statuses`]), the `Update` that corrects the copies the
//!   network holds.
//! - `DELETE /statuses/{id}` (`delete`) — the shared
//!   [`delete_status`] service, the same write the author's own delete and
//!   the expiration jobs run: tombstone, reblogs of it, counters, the
//!   notification cleanup, the federated withdrawal (the `Delete` of a
//!   note, the `Undo` of a boost) and the stream push. A `status_delete`
//!   [`moderation_log`] entry names the staff action, and the answer is
//!   Pleroma's `json(conn, %{})`: `200` with the empty JSON object. A row
//!   a concurrent delete took first is the 404.
//! - `GET /users/{nickname}/statuses` (`list_user_statuses`) — the account
//!   the segment names, by nickname (a remote `nick@domain` handle
//!   included) or id through [`user::admin::get_by_nickname_or_id`], its
//!   statuses through the same filters, window and entity. Beside the bare
//!   array of `GET /statuses` this answers Pleroma's `{total, activities}`
//!   envelope: its `list_user_statuses` renders the `StatusView` clause that
//!   carries a total, and `total` is the whole set the filters select, not
//!   the window. An unknown nickname is the same 404 as an unknown id.
//! - `GET /instances/{instance}/statuses` (`list_statuses`) — the same
//!   envelope, narrowed to the accounts mirrored from one domain; `local`
//!   is the literal for this instance's own accounts, which is
//!   `local_only` by another name and runs the same arm. A domain nobody
//!   came from answers the empty page rather than a miss — Pleroma's
//!   `instance` filter is a `WHERE`, not a lookup.
//!
//! The ids are this schema's `UUIDv7` text rather than Pleroma's integers
//! (decision 4), and the `page`/`page_size` window is read with the lenient
//! rules [`super`] states for every admin listing.
//!
//! [`StaffUser`]: crate::auth::admin::StaffUser
//! [`StaffUser::require`]: crate::auth::admin::StaffUser::require
//! [`Account`]: crate::entities::account::Account
//! [`delete_status`]: crate::services::delete_status::delete_status
//! [`moderation_log`]: crate::models::moderation_log
//! [`render_statuses`]: crate::services::render_status::render_statuses
//! [`status::admin::list`]: crate::models::status::admin::list
//! [`status::admin::set_scope`]: crate::models::status::admin::set_scope
//! [`status::get_by_id`]: crate::models::status::get_by_id
//! [`user::admin::get_by_nickname_or_id`]: crate::models::user::admin::get_by_nickname_or_id
//! [`ViewerMode::Admin`]: crate::services::render_status::ViewerMode::Admin

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use tracing::debug;

use crate::{
    auth::admin::StaffUser,
    entities::{admin::AdminStatusPage, status::Status},
    error::{AppError, AppResult},
    extract::{ApiBody, flex},
    models::{
        moderation_log,
        status::{self, Status as StatusRow, Visibility},
        user::{self, Flag, User},
    },
    services::render_status::{ViewerMode, render_status, render_statuses},
    state::AppState,
    util::id::parse_id,
};

/// The read endpoint class of [`StaffUser::require`].
const CLASS_READ: &str = "read";
/// The write endpoint class of [`StaffUser::require`].
const CLASS_WRITE: &str = "write";
/// The admin scope area these endpoints live in (`admin:read:statuses` and
/// `admin:write:statuses`, Pleroma's `OAuthScopesPlug` grants for the
/// status controller and the two status listings).
const AREA_STATUSES: &str = "statuses";
/// The 422 for a `visibility` off the five the column's `CHECK` constraint
/// holds (the ticket's wording over Pleroma's `CastAndValidate` refusal).
const INVALID_VISIBILITY: &str = "Invalid visibility";
/// The `instances/{instance}` literal that stands for this instance's own
/// accounts instead of a mirrored domain.
const LOCAL: &str = "local";
/// The spelling of a query boolean Pleroma's cast accepts as yes, and its
/// no; anything else is the 400 naming the parameter.
const TRUE_SPELLINGS: [&str; 2] = ["true", "1"];
/// [`TRUE_SPELLINGS`]'s negative pair.
const FALSE_SPELLINGS: [&str; 2] = ["false", "0"];

/// The status routes of the admin API, merged into [`super`]: Pleroma's
/// `AdminAPI.StatusController` and the two status actions beside it.
///
/// Do not call [`Router::fallback`] here (or on anything merged into this
/// tree): the app-wide JSON fallback in [`crate::app`] must answer the
/// unmatched paths.
///
/// [`Router::fallback`]: axum::Router::fallback
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/statuses", get(list_statuses))
        .route(
            "/statuses/{id}",
            get(show_status).put(update_status).delete(delete_status),
        )
        .route("/users/{nickname}/statuses", get(list_user_statuses))
        .route(
            "/instances/{instance}/statuses",
            get(list_instance_statuses),
        )
}

// ---------------------------------------------------------------------------
// the ledger
// ---------------------------------------------------------------------------

/// `GET /api/v1/pleroma/admin/statuses` — Pleroma's `index`: the filtered
/// window of the whole network's ledger as a bare array of [`Status`]
/// entities, newest first, with no total beside it.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:read` gate; [`AppError::BadRequest`] for a `godmode`,
/// `local_only` or `with_reblogs` that is no boolean; [`AppError::Database`]
/// from the listing read.
async fn list_statuses(
    State(state): State<AppState>,
    staff: StaffUser,
    Query(query): Query<HashMap<String, String>>,
) -> AppResult<Json<Vec<Status>>> {
    staff.require(CLASS_READ, AREA_STATUSES)?;
    let params = list_params(&query)?;
    let rows = status::admin::list(&state.db, &params).await?;
    debug!(
        shown = rows.len(),
        godmode = params.godmode.get(),
        local_only = params.local_only.get(),
        with_reblogs = params.with_reblogs.get(),
        "admin statuses listed"
    );
    Ok(Json(rendered(&state, &staff.0, &rows).await?))
}

/// `GET /api/v1/pleroma/admin/statuses/{id}` — Pleroma's `show`: the one
/// post the id names, seen through the admin view that asks no visibility
/// question.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:read` gate; [`AppError::RouteNotFound`] for a malformed or
/// unknown id (see [`live_status`]) and for the row the render cannot answer;
/// [`AppError::Database`] from the two reads.
async fn show_status(
    State(state): State<AppState>,
    staff: StaffUser,
    Path(raw): Path<String>,
) -> AppResult<Json<Status>> {
    staff.require(CLASS_READ, AREA_STATUSES)?;
    let row = live_status(&state.db, &raw).await?;
    debug!(status = %row.id, "admin status shown");
    one(&state, &staff.0, &row).await.map(Json)
}

/// `PUT /api/v1/pleroma/admin/statuses/{id}` — Pleroma's `update`: the two
/// scope fields the body presents, the `status_update` log entry naming
/// them, the re-scoped entity, and the `Update` the network is owed for a
/// post of this instance.
///
/// The hook is the edit route's own `after_status_edited`
/// ([`crate::api::statuses`]), handed the pre-write row exactly as that
/// route hands it: the telling reads the author and the audience off the
/// snapshot and re-reads the post the write stored. It runs for a local row
/// alone — a mirrored post belongs to its origin's `Update`, and Pleroma's
/// federation says nothing here for one.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:write` gate; [`AppError::RouteNotFound`] for a malformed or
/// unknown id, for a row a concurrent delete took between the gate and the
/// write, and for the row the render cannot answer;
/// [`AppError::Unprocessable`] for a `visibility` the column would not hold;
/// [`AppError::Database`] from the write and the log.
async fn update_status(
    State(state): State<AppState>,
    staff: StaffUser,
    Path(raw): Path<String>,
    ApiBody(body): ApiBody<ScopeBody>,
) -> AppResult<Json<Status>> {
    staff.require(CLASS_WRITE, AREA_STATUSES)?;
    let row = live_status(&state.db, &raw).await?;
    let visibility = requested_visibility(body.visibility.as_deref())?;
    let updated = status::admin::set_scope(&state.db, row.id, body.sensitive, visibility)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    let logged = moderation_log::status_update(
        &state.db,
        &staff.0,
        row.id,
        body.sensitive,
        body.visibility.as_deref(),
    )
    .await?;
    debug!(
        log = %logged.id,
        status = %updated.id,
        sensitive = updated.sensitive,
        visibility = %updated.visibility,
        "admin status scope updated"
    );
    if updated.local {
        crate::api::statuses::after_status_edited(&state, &row).await;
    }
    one(&state, &staff.0, &updated).await.map(Json)
}

/// `DELETE /api/v1/pleroma/admin/statuses/{id}` — Pleroma's `delete`: the
/// shared deletion service, the `status_delete` log entry beside it, and
/// the empty JSON object as the answer.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:write` gate; [`AppError::RouteNotFound`] for a malformed or
/// unknown id and for the row a concurrent delete reached first;
/// [`AppError::Database`] from the write and the log.
async fn delete_status(
    State(state): State<AppState>,
    staff: StaffUser,
    Path(raw): Path<String>,
) -> AppResult<Json<Value>> {
    staff.require(CLASS_WRITE, AREA_STATUSES)?;
    let row = live_status(&state.db, &raw).await?;
    if !crate::services::delete_status::delete_status(&state, &row).await? {
        return Err(AppError::RouteNotFound);
    }
    let logged = moderation_log::status_delete(&state.db, &staff.0, row.id).await?;
    debug!(log = %logged.id, status = %row.id, "admin status deleted");
    Ok(Json(json!({})))
}

// ---------------------------------------------------------------------------
// one account's and one domain's page
// ---------------------------------------------------------------------------

/// `GET /api/v1/pleroma/admin/users/{nickname}/statuses` — Pleroma's
/// `list_user_statuses`: that account's statuses through the same filters
/// and window, answered as the `{total, activities}` envelope.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:read` gate; [`AppError::RouteNotFound`] for text that is
/// no stored nickname and no id; [`AppError::BadRequest`] for an unusable
/// filter; [`AppError::Database`] from the two reads.
async fn list_user_statuses(
    State(state): State<AppState>,
    staff: StaffUser,
    Query(query): Query<HashMap<String, String>>,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminStatusPage>> {
    staff.require(CLASS_READ, AREA_STATUSES)?;
    let target = user::admin::get_by_nickname_or_id(&state.db, &raw)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    let params = status::admin::ListParams {
        user_id: Some(target.id),
        ..list_params(&query)?
    };
    envelope(&state, &staff.0, &params).await.map(Json)
}

/// `GET /api/v1/pleroma/admin/instances/{instance}/statuses` — Pleroma's
/// instance `list_statuses`: the statuses of the accounts mirrored from one
/// domain (or, under the `local` literal, of this instance's own accounts),
/// through the same filters and window and in the same envelope.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:read` gate; [`AppError::BadRequest`] for an unusable
/// filter; [`AppError::Database`] from the two reads.
async fn list_instance_statuses(
    State(state): State<AppState>,
    staff: StaffUser,
    Query(query): Query<HashMap<String, String>>,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminStatusPage>> {
    staff.require(CLASS_READ, AREA_STATUSES)?;
    let open = list_params(&query)?;
    let params = if raw == LOCAL {
        status::admin::ListParams {
            local_only: Flag::new(true),
            ..open
        }
    } else {
        status::admin::ListParams {
            domain: Some(raw),
            ..open
        }
    };
    envelope(&state, &staff.0, &params).await.map(Json)
}

/// One `{total, activities}` answer: the whole count the filters select,
/// beside that window of rows rendered through the admin view. The count
/// runs first, as Pleroma's `total: true` aggregation does — a page is
/// never answered with a total of a different set.
///
/// # Errors
///
/// [`AppError::Database`] from either read.
async fn envelope(
    state: &AppState,
    staff: &User,
    params: &status::admin::ListParams,
) -> AppResult<AdminStatusPage> {
    let total = status::admin::count(&state.db, params).await?;
    let rows = status::admin::list(&state.db, params).await?;
    debug!(
        shown = rows.len(),
        total,
        page = params.page,
        "admin status page listed"
    );
    Ok(AdminStatusPage {
        total,
        activities: rendered(state, staff, &rows).await?,
    })
}

// ---------------------------------------------------------------------------
// parameters and rendering
// ---------------------------------------------------------------------------

/// The three listings' shared filters and window: the lenient `page` /
/// `page_size` reading of [`super`], and the three boolean filters, each
/// absent-or-default-false and any other text the 400 that names it.
///
/// # Errors
///
/// [`AppError::BadRequest`] for a non-boolean filter value.
fn list_params(params: &HashMap<String, String>) -> AppResult<status::admin::ListParams> {
    Ok(status::admin::ListParams {
        page: super::page_number(params, "page", super::DEFAULT_PAGE),
        page_size: super::page_size(params),
        godmode: Flag::new(query_bool(params, "godmode")?),
        local_only: Flag::new(query_bool(params, "local_only")?),
        with_reblogs: Flag::new(query_bool(params, "with_reblogs")?),
        user_id: None,
        domain: None,
    })
}

/// One boolean query filter, read the way the admin API reads its other
/// boolean parameters (see [`super::apps_docs`]'s `trusted`): the same
/// spellings [`flex::bool`] accepts in a body, absent meaning the default,
/// and anything else a 400 naming the parameter.
///
/// # Errors
///
/// [`AppError::BadRequest`] for a value that is neither spelling.
fn query_bool(params: &HashMap<String, String>, key: &str) -> AppResult<bool> {
    let Some(text) = super::text_of(params, key) else {
        return Ok(false);
    };
    let lowered = text.to_ascii_lowercase();
    if TRUE_SPELLINGS.contains(&lowered.as_str()) {
        return Ok(true);
    }
    if FALSE_SPELLINGS.contains(&lowered.as_str()) {
        return Ok(false);
    }
    Err(AppError::BadRequest(format!(
        "{key} must be a boolean, not {text}"
    )))
}

/// The body of `PUT /api/v1/pleroma/admin/statuses/{id}`: Pleroma's two
/// scope fields, each optional — `sensitive` through [`flex::opt_bool`] so a
/// form-encoded `"true"` counts, `visibility` as the text the column holds,
/// which [`requested_visibility`] weighs.
#[derive(Debug, Deserialize)]
struct ScopeBody {
    /// Whether the content warning folds the post away: `None` leaves the
    /// stored flag alone.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    sensitive: Option<bool>,
    /// Who may see the post: `None` leaves the stored visibility alone.
    #[serde(default)]
    visibility: Option<String>,
}

/// The submitted `visibility` as the [`Visibility`] it names, refused with
/// the ticket's 422 before anything is written — Pleroma's
/// `CastAndValidate` answers the same refusal for a scope its
/// `VisibilityScope` schema does not list, and a partial write of a bad
/// scope is worse than none.
///
/// # Errors
///
/// [`AppError::Unprocessable`] for a text off the five.
fn requested_visibility(text: Option<&str>) -> AppResult<Option<Visibility>> {
    let Some(text) = text else {
        return Ok(None);
    };
    text.parse::<Visibility>()
        .map(Some)
        .map_err(|_unknown| AppError::Unprocessable(INVALID_VISIBILITY.to_owned()))
}

/// The `{id}` (or `{nickname}` resolved elsewhere) segment of a status
/// route as its live row: a malformed id, an unknown one and a tombstone
/// are the one 404 Pleroma's fallback answers for a miss.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] as described; any database failure arrives
/// through `?` as [`AppError::Database`].
async fn live_status(pool: &PgPool, text: &str) -> AppResult<StatusRow> {
    let Some(id) = parse_id(text) else {
        return Err(AppError::RouteNotFound);
    };
    status::get_by_id(pool, id)
        .await?
        .ok_or(AppError::RouteNotFound)
}

/// `rows` as the [`Status`] entities of the admin view: the staff member's
/// own flags, the visibility rules bypassed.
///
/// # Errors
///
/// [`AppError::Database`] from the batch's reads.
async fn rendered(state: &AppState, staff: &User, rows: &[StatusRow]) -> AppResult<Vec<Status>> {
    render_statuses(state, ViewerMode::Admin(staff), rows).await
}

/// One `row` as the admin view's [`Status`] entity, the row a render cannot
/// answer for (the orphaned boost the shared renderer drops) being the same
/// 404 as a row that is not there.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] as described; [`AppError::Database`] from the
/// batch's reads.
async fn one(state: &AppState, staff: &User, row: &StatusRow) -> AppResult<Status> {
    render_status(state, ViewerMode::Admin(staff), row)
        .await?
        .ok_or(AppError::RouteNotFound)
}

#[cfg(test)]
mod tests {
    use axum::response::IntoResponse as _;

    use super::*;

    /// One parsed pair of query parameters, for the filter tests.
    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn the_filters_default_to_the_public_ledger() {
        let parsed = list_params(&params(&[])).expect("an empty query is parsable");
        assert_eq!(parsed.page, super::super::DEFAULT_PAGE, "page starts at 1");
        assert_eq!(parsed.page_size, 50, "and the window is 50 rows");
        assert!(
            !parsed.godmode.get() && !parsed.local_only.get() && !parsed.with_reblogs.get(),
            "every boolean filter is off by default, so the ledger is the \
             public one without boosts"
        );
        assert!(
            parsed.user_id.is_none() && parsed.domain.is_none(),
            "and nothing narrows the audience"
        );
        let junk = list_params(&params(&[("page", "nope"), ("page_size", "-1")]))
            .expect("unusable numbers fall to the defaults");
        assert_eq!(junk.page_size, 50, "a broken page_size is the default");
    }

    #[test]
    fn the_page_size_keeps_the_admin_cap() {
        let parsed = list_params(&params(&[("page", "3"), ("page_size", "5000")])).expect("parses");
        assert_eq!(parsed.page, 3, "the page passes through");
        assert_eq!(
            parsed.page_size,
            super::super::MAX_PAGE_SIZE,
            "and the size is capped like every other admin listing"
        );
    }

    #[test]
    fn the_boolean_filters_read_their_spellings() {
        for (spelling, want) in [
            ("true", true),
            ("TRUE", true),
            ("1", true),
            ("false", false),
            ("0", false),
        ] {
            let parsed = list_params(&params(&[
                ("godmode", spelling),
                ("with_reblogs", spelling),
            ]))
            .expect("a listed spelling reads");
            assert_eq!(
                parsed.godmode.get(),
                want,
                "{spelling} is {want} for godmode"
            );
            assert_eq!(
                parsed.with_reblogs.get(),
                want,
                "{spelling} is {want} for with_reblogs"
            );
        }
    }

    #[test]
    fn an_unusable_boolean_names_its_parameter() {
        for key in ["godmode", "local_only", "with_reblogs"] {
            let err = list_params(&params(&[(key, "yes-and-no")]))
                .expect_err("no boolean reads that text");
            assert_eq!(
                err.to_string(),
                format!("{key} must be a boolean, not yes-and-no"),
                "the refusal names the parameter and repeats its text"
            );
            let response = err.into_response();
            assert_eq!(
                response.status(),
                axum::http::StatusCode::BAD_REQUEST,
                "and it is a 400"
            );
        }
    }

    #[test]
    fn a_known_visibility_reads_and_an_unknown_one_is_the_422() {
        assert_eq!(
            requested_visibility(None).expect("absent is no change"),
            None,
            "an absent visibility leaves the column alone"
        );
        for text in ["public", "unlisted", "private", "direct", "local"] {
            let want = text.parse::<Visibility>().expect("a stored spelling");
            assert_eq!(
                requested_visibility(Some(text)).expect("the five read"),
                Some(want),
                "{text} is one of the five"
            );
        }
        for text in ["", "Public", "everything", "all"] {
            let err = requested_visibility(Some(text)).expect_err("nothing else is a visibility");
            assert_eq!(
                err.to_string(),
                INVALID_VISIBILITY,
                "the refusal is the ticket's own wording"
            );
            let response = err.into_response();
            assert_eq!(
                response.status(),
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                "and it is a 422"
            );
        }
    }
}
