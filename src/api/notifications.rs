//! Flat notifications over HTTP: the Mastodon v1 dismissal routes and
//! Pleroma's `read`, on top of the rows owned by
//! [`crate::models::notification`] (decision 3).
//!
//! The reference is Pleroma's `NotificationController`
//! (`lib/pleroma/web/mastodon_api/controllers/notification_controller.ex`,
//! the `clear`, `dismiss_via_body` and `destroy_multiple` actions) and its
//! `PleromaAPI.NotificationController.mark_as_read`
//! (`lib/pleroma/web/pleroma_api/controllers/notification_controller.ex`).
//! Where Pleroma hard-deletes the rows, dismissal here is soft: it stamps
//! [`dismissed_at`](NotificationRow::dismissed_at), and every read — the
//! grouped v2 listings and these routes' own writes — hides the stamped
//! rows, which is what the client observes either way.
//!
//! - `POST /api/v1/notifications/clear` dismisses all of the caller's
//!   undismissed notifications and answers `{}` (Pleroma's `Notification.clear`,
//!   soft).
//! - `POST /api/v1/notifications/{id}/dismiss` dismisses the one
//!   notification `id` names; a malformed, unknown or foreign id is the
//!   shared 404 — Pleroma's `"Cannot dismiss notification"`, which the
//!   ticket renders as 404 rather than its 403. A repeat dismissal of an
//!   owned row answers `{}` again and keeps the first stamp.
//! - `POST /api/v1/notifications/dismiss` is the same by body `id`
//!   (Pleroma's deprecated `dismiss_via_body`); a body without `id` is a
//!   400 `{"error":"Missing parameter: id"}`.
//! - `DELETE /api/v1/notifications/destroy_multiple` dismisses the listed
//!   ids that belong to the caller — `ids[]=a&ids[]=b` in the query
//!   string, a JSON or form body, or both merged (decision 10) — and
//!   answers `{}`. Foreign, unknown and malformed ids simply match
//!   nothing.
//! - `POST /api/pleroma/notifications/read` marks seen by Pleroma's two
//!   clauses: `id` marks that notification and everything older
//!   (decision 4: id order is time order) and answers the single
//!   notification entity — 404 when the id names no row the caller owns;
//!   `max_id` marks all of the caller's notifications up to and including
//!   it and answers the array of the rows it changed, newest first
//!   (Pleroma's `set_read_up_to`). `id` wins when both arrive, Pleroma's
//!   clause order. Neither is a 400 `{"error":"Missing parameter: id or
//!   max_id"}`; a `max_id` that is not id text is the pagination wording's
//!   400 `"Invalid id"`.
//!
//! Every route is authenticated ([`AuthUser`]) and requires
//! `write:notifications` — all five change state. The deleted-status
//! cleanup that keeps these listings honest runs off the delete route's
//! `after_status_deleted` hook in [`crate::api::statuses`].

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Path, State},
    response::{IntoResponse, Response},
    routing::{delete, post},
};
use serde::Deserialize;
use serde_json::{Map, Value};
use tracing::debug;
use uuid::Uuid;

use crate::{
    auth::AuthUser,
    entities::{
        account::{Account, AccountViewer},
        notification::Notification as NotificationEntity,
        relationship::{Relationship, RelationshipMap},
    },
    error::{AppError, AppResult},
    extract::{ApiBody, ApiParams},
    models::{
        notification::{self, Notification as NotificationRow},
        status::{self, Status as StatusRow},
        status_visibility,
        user::{self, User},
    },
    oauth::scopes::WRITE_NOTIFICATIONS,
    services::render_status::render_statuses,
    state::AppState,
    util::id::parse_id,
};

/// The 400 body of the legacy dismissal given no `id`.
const MISSING_ID: &str = "Missing parameter: id";
/// The 400 body of `read` given neither `id` nor `max_id`.
const MISSING_ID_OR_MAX_ID: &str = "Missing parameter: id or max_id";
/// The 400 body for a cursor that is not id text — the wording of
/// [`crate::pagination`] for the same failure.
const INVALID_ID: &str = "Invalid id";

/// The `/api/v1` dismissal router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/notifications/clear", post(clear))
        .route("/notifications/dismiss", post(dismiss_via_body))
        .route("/notifications/{id}/dismiss", post(dismiss))
        .route("/notifications/destroy_multiple", delete(destroy_multiple))
}

/// The `/api/pleroma/notifications` router, merged into
/// [`crate::api::pleroma`].
pub fn pleroma_router() -> Router<AppState> {
    Router::new().route("/read", post(mark_as_read))
}

/// The `{}` answer of the dismissal routes (Pleroma's `json(conn, %{})`).
fn empty_body() -> Json<Value> {
    Json(Value::Object(Map::new()))
}

/// `POST /api/v1/notifications/clear` — dismiss all of the caller's
/// notifications and answer `{}`. There is nothing to 404: an account
/// with nothing to dismiss changes nothing and still gets the empty
/// object, Pleroma's unconditional render.
///
/// # Errors
///
/// [`Unauthorized`](crate::error::AppError::Unauthorized) /
/// [`Forbidden`](crate::error::AppError::Forbidden) from [`AuthUser`] and
/// the `write:notifications` check. Any database failure arrives through
/// `?` as [`Database`](crate::error::AppError::Database).
async fn clear(State(state): State<AppState>, auth: AuthUser) -> AppResult<Json<Value>> {
    auth.require(WRITE_NOTIFICATIONS)?;
    let dismissed = notification::dismiss_all(&state.db, auth.0.id).await?;
    debug!(
        dismissed,
        user = %auth.0.id,
        "notifications cleared"
    );
    Ok(empty_body())
}

/// `POST /api/v1/notifications/{id}/dismiss` — dismiss the one
/// notification and answer `{}`; Pleroma's `dismiss`, whose foreign- or
/// missing-row refusal the ticket renders as 404.
///
/// # Errors
///
/// The auth errors of [`clear`]; [`NotFound`](crate::error::AppError::NotFound)
/// for an id that is malformed, unknown or somebody else's.
async fn dismiss(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_NOTIFICATIONS)?;
    dismiss_one(&state, auth.0.id, &id).await?;
    Ok(empty_body())
}

/// `POST /api/v1/notifications/dismiss` — the deprecated body-`id` twin of
/// [`dismiss`] (Pleroma's `dismiss_via_body`), answering the same thing
/// for the same id and a 400 `Missing parameter: id` for a body that
/// carries none.
///
/// # Errors
///
/// The auth errors of [`clear`]; [`BadRequest`](crate::error::AppError::BadRequest)
/// without `id`; [`NotFound`](crate::error::AppError::NotFound) as for
/// [`dismiss`].
async fn dismiss_via_body(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<DismissParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_NOTIFICATIONS)?;
    let id = params.required_id()?;
    dismiss_one(&state, auth.0.id, id).await?;
    Ok(empty_body())
}

/// The shared dismissal write of both dismiss routes: the 404 covers a
/// malformed id, an unknown one and a foreign row in one indistinguishable
/// answer, and a row the caller owns comes back stamped (a repeat
/// dismissal keeps the first stamp and answers `{}` again).
///
/// # Errors
///
/// [`NotFound`](crate::error::AppError::NotFound) as described.
async fn dismiss_one(state: &AppState, user_id: Uuid, id_text: &str) -> AppResult<()> {
    let Some(id) = parse_id(id_text) else {
        return Err(AppError::NotFound);
    };
    if notification::dismiss(&state.db, user_id, id)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound);
    }
    debug!(
        notification = %id,
        %user_id,
        "notification dismissed"
    );
    Ok(())
}

/// The body of the legacy dismissal: `id`, required.
#[derive(Debug, Deserialize)]
struct DismissParams {
    /// The notification to dismiss, as id text; absent asks for nothing
    /// and is the 400.
    #[serde(default)]
    id: Option<String>,
}

impl DismissParams {
    /// The `id` text, or the 400 the endpoint owes a body without it.
    ///
    /// # Errors
    ///
    /// [`BadRequest`](crate::error::AppError::BadRequest) with
    /// [`MISSING_ID`].
    fn required_id(&self) -> AppResult<&str> {
        self.id
            .as_deref()
            .ok_or_else(|| AppError::BadRequest(MISSING_ID.to_owned()))
    }
}

/// `DELETE /api/v1/notifications/destroy_multiple` — dismiss the listed
/// notifications that belong to the caller and answer `{}`. Ids the
/// caller does not own, that name nothing, or that are no id text at all
/// simply match nothing; asking with no `ids` at all dismisses nothing and
/// is likewise not an error.
///
/// # Errors
///
/// The auth errors of [`clear`]. Any database failure arrives through `?`
/// as [`Database`](crate::error::AppError::Database).
async fn destroy_multiple(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiParams(params): ApiParams<DestroyParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_NOTIFICATIONS)?;
    let ids = params.parsed_ids();
    let dismissed = notification::dismiss_ids(&state.db, auth.0.id, &ids).await?;
    debug!(
        dismissed,
        requested = ids.len(),
        user = %auth.0.id,
        "notifications dismissed in bulk"
    );
    Ok(empty_body())
}

/// The bulk dismissal's parameters: `ids` under either spelling clients
/// send — a list (`ids[]=a&ids[]=b`, or a JSON array) or a lone value —
/// from the query string, the body, or both merged.
#[derive(Debug, Deserialize)]
struct DestroyParams {
    /// The notifications to dismiss; absent asks for none.
    #[serde(default)]
    ids: Option<IdList>,
}

/// One value or a list (a JSON array, or `ids[]=…` keys) — the two
/// spellings Mastodon clients and forms both send.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IdList {
    /// A lone id.
    Single(String),
    /// A list of ids.
    Multiple(Vec<String>),
}

impl IdList {
    /// The values, borrowed, in the order sent.
    fn values(&self) -> Vec<&str> {
        match self {
            Self::Single(value) => vec![value.as_str()],
            Self::Multiple(values) => values.iter().map(String::as_str).collect(),
        }
    }
}

impl DestroyParams {
    /// The parsed ids, in the order sent, each named once; values that
    /// are not id text are skipped — they can name no row anyway.
    fn parsed_ids(&self) -> Vec<Uuid> {
        let values = self.ids.as_ref().map_or_else(Vec::new, IdList::values);
        let mut ids: Vec<Uuid> = Vec::new();
        for value in values {
            let Some(id) = parse_id(value) else {
                continue;
            };
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        ids
    }
}

/// `POST /api/pleroma/notifications/read` — mark seen and answer per the
/// requested target ([`ReadParams::target`]).
///
/// # Errors
///
/// The auth errors of [`clear`]; [`BadRequest`](crate::error::AppError::BadRequest)
/// for neither parameter, or for a `max_id` that is not id text;
/// [`NotFound`](crate::error::AppError::NotFound) for an `id` naming no
/// row the caller owns.
async fn mark_as_read(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<ReadParams>,
) -> AppResult<ReadAnswer> {
    auth.require(WRITE_NOTIFICATIONS)?;
    match params.target()? {
        ReadTarget::One(id_text) => read_one(&state, &auth.0, id_text).await,
        ReadTarget::UpTo(max_text) => read_up_to(&state, &auth.0, max_text).await,
    }
}

/// The body of `read`: `id`, `max_id`, or the 400.
#[derive(Debug, Deserialize)]
struct ReadParams {
    /// The notification whose age closes the read set, as id text.
    #[serde(default)]
    id: Option<String>,
    /// The cursor the read set closes at, as id text.
    #[serde(default)]
    max_id: Option<String>,
}

/// What one `read` call marks, borrowed from [`ReadParams`].
#[derive(Debug)]
enum ReadTarget<'a> {
    /// The notification `id` names, plus everything older.
    One(&'a str),
    /// Everything up to and including the `max_id` cursor.
    UpTo(&'a str),
}

impl ReadParams {
    /// The requested target, `id` first when both arrive — Pleroma's
    /// clause order — and neither is the 400 the ticket words.
    ///
    /// # Errors
    ///
    /// [`BadRequest`](crate::error::AppError::BadRequest) with
    /// [`MISSING_ID_OR_MAX_ID`].
    fn target(&self) -> AppResult<ReadTarget<'_>> {
        if let Some(id) = self.id.as_deref() {
            return Ok(ReadTarget::One(id));
        }
        if let Some(max_id) = self.max_id.as_deref() {
            return Ok(ReadTarget::UpTo(max_id));
        }
        Err(AppError::BadRequest(MISSING_ID_OR_MAX_ID.to_owned()))
    }
}

/// The `read` answer: the one entity an `id` names, or the array of the
/// rows a `max_id` marked (the two shapes the two clauses render).
#[derive(Debug)]
enum ReadAnswer {
    /// The entity of the read notification, boxed so the two arms keep
    /// one small size.
    One(Box<NotificationEntity>),
    /// The entities of the marked notifications, newest first.
    Many(Vec<NotificationEntity>),
}

impl IntoResponse for ReadAnswer {
    fn into_response(self) -> Response {
        match self {
            Self::One(entity) => Json(*entity).into_response(),
            Self::Many(entities) => Json(entities).into_response(),
        }
    }
}

/// The `id` clause: read the caller's notification `id_text` and
/// everything older, and answer its entity in its now-seen state.
///
/// # Errors
///
/// [`NotFound`](crate::error::AppError::NotFound) for an id that is
/// malformed, unknown or somebody else's (one answer, Pleroma's `get/2`
/// finding nothing to read).
async fn read_one(state: &AppState, viewer: &User, id_text: &str) -> AppResult<ReadAnswer> {
    let Some(id) = parse_id(id_text) else {
        return Err(AppError::NotFound);
    };
    let mut row = notification::get(&state.db, viewer.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let marked = notification::mark_read_through_id(&state.db, viewer.id, id).await?;
    row.seen = true;
    debug!(
        notification = %id,
        marked = marked.len(),
        user = %viewer.id,
        "notification read"
    );
    let entities = render_notifications(state, viewer, std::slice::from_ref(&row)).await?;
    let entity = entities.into_iter().next().ok_or(AppError::NotFound)?;
    Ok(ReadAnswer::One(Box::new(entity)))
}

/// The `max_id` clause: read everything of the caller's up to and
/// including the cursor, and answer the entities of the rows this call
/// marked, newest first. A cursor that names no unseen notification —
/// somebody else's, a gone one, or one already read — marks nothing and
/// answers `[]`.
///
/// # Errors
///
/// [`BadRequest`](crate::error::AppError::BadRequest) with
/// [`INVALID_ID`] for a cursor that is not id text.
async fn read_up_to(state: &AppState, viewer: &User, max_text: &str) -> AppResult<ReadAnswer> {
    let Some(max_id) = parse_id(max_text) else {
        return Err(AppError::BadRequest(INVALID_ID.to_owned()));
    };
    let marked = notification::mark_read_through_id(&state.db, viewer.id, max_id).await?;
    debug!(
        count = marked.len(),
        user = %viewer.id,
        "notifications marked read"
    );
    let entities = render_notifications(state, viewer, &marked).await?;
    Ok(ReadAnswer::Many(entities))
}

/// Renders `rows` as notification entities for `viewer`: every distinct
/// actor expanded once into an [`Account`] with the viewer's relationship
/// embedded, every distinct status the rows name rendered for the viewer
/// through the shared batched path (a status that is gone or hidden from
/// this reader simply leaves the entity's `status` key out), and the
/// viewer's mutes read from the same relationship load that feeds the
/// account embeds. A row whose actor left no row (deleted mid-request)
/// drops out of the answer.
///
/// # Errors
///
/// [`Database`](crate::error::AppError::Database) from the account,
/// relationship, status and visibility reads.
async fn render_notifications(
    state: &AppState,
    viewer: &User,
    rows: &[NotificationRow],
) -> AppResult<Vec<NotificationEntity>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let actor_ids = distinct_ids(rows.iter().map(|row| row.from_account_id));
    let status_ids = distinct_ids(rows.iter().filter_map(|row| row.status_id));
    let users = user::get_many(&state.db, &actor_ids)
        .await?
        .into_iter()
        .map(|found| (found.id, found))
        .collect::<HashMap<Uuid, User>>();
    let relationships = Relationship::load_many(&state.db, viewer.id, &actor_ids)
        .await?
        .into_iter()
        .zip(actor_ids)
        .map(|(relationship, target)| (target, relationship))
        .collect::<RelationshipMap>();
    let statuses = visible_statuses(state, Some(viewer), &status_ids).await?;
    let mut entities = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(actor) = users.get(&row.from_account_id) else {
            continue;
        };
        let status = row.status_id.and_then(|id| statuses.get(&id).cloned());
        entities.push(NotificationEntity::from_row(
            row,
            Account::from_user_for(Some(viewer), actor, &state.config, Some(&relationships)),
            status,
            relationships
                .get(&row.from_account_id)
                .is_some_and(|found| found.muting.0),
        ));
    }
    Ok(entities)
}

/// The distinct status ids of `ids`, rendered as seen by `viewer`,
/// serialized, and keyed by their row ids: rows that name no row, that a
/// delete took, or that [`status_visibility`] hides from this reader are
/// not in the map (the rendered [`Status`] entity does not duplicate
/// cheaply, and several notifications can name one status, so the map
/// holds its JSON shape).
///
/// # Errors
///
/// [`Database`](crate::error::AppError::Database) from the status and
/// visibility reads;
/// [`Internal`](crate::error::AppError::Internal) if a rendered entity
/// would not serialize.
///
/// [`Status`]: crate::entities::status::Status
async fn visible_statuses(
    state: &AppState,
    viewer: AccountViewer<'_>,
    ids: &[Uuid],
) -> AppResult<HashMap<Uuid, Value>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = status::get_many(&state.db, ids).await?;
    let mut visible: Vec<StatusRow> = Vec::new();
    for row in rows {
        if status_visibility::can_view(&state.db, &row, viewer).await? {
            visible.push(row);
        }
    }
    let rendered = render_statuses(state, viewer, &visible).await?;
    let mut statuses = HashMap::new();
    for entity in rendered {
        let Some(id) = parse_id(&entity.id) else {
            continue;
        };
        let value = serde_json::to_value(entity).map_err(|err| {
            AppError::Internal(format!("status entity failed to serialize: {err}"))
        })?;
        let _first = statuses.insert(id, value);
    }
    Ok(statuses)
}

/// The values in order, each named once.
fn distinct_ids(values: impl Iterator<Item = Uuid>) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = Vec::new();
    for id in values {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Deserializes `value` as the endpoint's parameters, as the extractor
    /// would.
    fn params<T: serde::de::DeserializeOwned>(value: Value) -> T {
        crate::extract::into_params(value).expect("the fixture deserializes")
    }

    #[test]
    fn the_legacy_dismissal_asks_for_its_id() {
        let present: DismissParams = params(json!({"id": "01934567-89ab-7cde-f012-3456789abcde"}));
        assert_eq!(
            present.required_id().expect("the id was sent"),
            "01934567-89ab-7cde-f012-3456789abcde",
            "the text arrives unchanged"
        );
        for body in [json!({}), json!({"id": null}), json!({"other": 1})] {
            let absent: DismissParams = params(body.clone());
            let err = absent
                .required_id()
                .expect_err(&format!("`{body}` carries no id"));
            assert_eq!(err.to_string(), MISSING_ID, "the ticket's wording");
            assert!(
                matches!(err, AppError::BadRequest(_)),
                "which is a 400, not {err}"
            );
        }
    }

    #[test]
    fn the_bulk_ids_parse_from_lists_and_lone_values() {
        let id = "01934567-89ab-7cde-f012-3456789abcde";
        let other = "01934567-89ab-7cde-f012-3456789abcdf";
        let list: DestroyParams = params(json!({"ids": [id, other, id]}));
        assert_eq!(
            list.parsed_ids(),
            vec![
                id.parse::<Uuid>().expect("fixture id"),
                other.parse().expect("fixture id")
            ],
            "the json array, each id once"
        );
        let lone: DestroyParams = params(json!({"ids": id}));
        assert_eq!(lone.parsed_ids().len(), 1, "and the lone `ids=` value");
        let mixed: DestroyParams = params(json!({"ids": [id, "not-an-id", "7"]}));
        assert_eq!(
            mixed.parsed_ids().len(),
            1,
            "values that name no row are skipped, not an error"
        );
        let empty: DestroyParams = params(json!({}));
        assert!(empty.parsed_ids().is_empty(), "absent asks for none");
    }

    #[test]
    fn read_targets_prefer_the_id_and_name_the_missing_one() {
        let both: ReadParams = params(json!({"id": "a", "max_id": "b"}));
        assert!(
            matches!(both.target().expect("id wins"), ReadTarget::One("a")),
            "`id` first, Pleroma's clause order"
        );
        let cursor: ReadParams = params(json!({"max_id": "b"}));
        assert!(
            matches!(cursor.target().expect("max_id"), ReadTarget::UpTo("b")),
            "the cursor alone is the array clause"
        );
        let neither: ReadParams = params(json!({}));
        let err = neither.target().expect_err("no target was asked for");
        assert_eq!(
            err.to_string(),
            MISSING_ID_OR_MAX_ID,
            "the ticket's wording"
        );
        assert!(
            matches!(err, AppError::BadRequest(_)),
            "which is a 400, not {err}"
        );
    }
}
