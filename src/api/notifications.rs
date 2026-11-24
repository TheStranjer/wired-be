//! Flat notifications over HTTP: the Mastodon v1 listing and read, the
//! v1 dismissal routes and Pleroma's `read`, on top of the rows owned by
//! [`crate::models::notification`] (decision 3).
//!
//! The reference is Pleroma's `NotificationController`
//! (`lib/pleroma/web/mastodon_api/controllers/notification_controller.ex`,
//! the `index`, `show`, `clear`, `dismiss_via_body` and `destroy_multiple`
//! actions) and its `PleromaAPI.NotificationController.mark_as_read`
//! (`lib/pleroma/web/pleroma_api/controllers/notification_controller.ex`),
//! with the listing's row scope from `Pleroma.Notification.for_user_query`
//! (`lib/pleroma/notification.ex`).
//! Where Pleroma hard-deletes the rows, dismissal here is soft: it stamps
//! [`dismissed_at`](NotificationRow::dismissed_at), and every read — the
//! grouped v2 listings and these routes' own writes — hides the stamped
//! rows, which is what the client observes either way.
//!
//! - `GET /api/v1/notifications` — one [`PageParams`] page (default 20,
//!   at most 40) of the caller's notifications, newest first, rendered as
//!   notification entities with the `Link` header keyed on the page's row
//!   ids. `types[]` restricts and `exclude_types[]` removes (the Pleroma/
//!   Mastodon alias `include_types[]` feeds `types[]` when that is absent;
//!   a type string no row carries simply matches nothing, it is not an
//!   error), and `account_id` keeps one actor's rows. Dismissed rows and
//!   rows of deactivated actors never appear; with `with_muted` left
//!   false the caller's muted actors, muted threads, blocked actors and
//!   blocked domains are hidden too — and Pleroma's `with_muted` never
//!   uncovers blocks. The caller's active filters then apply in the
//!   `notifications` context: a notification whose rendered status a
//!   warning filter matched carries the status's `filtered` annotation,
//!   and one an `irreversible` filter matched leaves the listing with it
//!   (see [`crate::services::filter`]). The
//!   `pleroma.is_seen` flag rides on the row's
//!   `seen` flag.
//! - `GET /api/v1/notifications/{id}` — the one notification, owner
//!   only; a dismissed, foreign, unknown or malformed id is the shared
//!   404 `{"error":"Record not found"}`.
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
//! Every route is authenticated ([`AuthUser`]); the two reads require
//! `read:notifications` and the five write routes `write:notifications`.
//! The deleted-status cleanup that keeps these listings honest runs off
//! the delete route's `after_status_deleted` hook in
//! [`crate::api::statuses`].

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Deserialize;
use serde_json::{Map, Value};
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::{
        chats::message_of,
        helpers::distinct,
        notifications_v2::{StringList, account_filter, names_of},
    },
    auth::AuthUser,
    entities::{
        account::{Account, AccountViewer},
        notification::Notification as NotificationEntity,
        relationship::{Relationship, RelationshipMap},
    },
    error::{AppError, AppResult},
    extract::{ApiBody, ApiParams, flex},
    models::{
        chat,
        filter::{Filter, list_active},
        notification::{
            self, FlatPageParams, Notification as NotificationRow, NotificationFilters,
        },
        status::{self, Status as StatusRow},
        status_visibility,
        user::{self, User},
    },
    oauth::scopes::{READ_NOTIFICATIONS, WRITE_NOTIFICATIONS},
    pagination::{NO_EXTRA_QUERY, PageParams, keyset_link, set_link_header},
    services::{
        filter::{FilterContext, apply_filters},
        render_status::{ViewerMode, render_statuses},
    },
    state::AppState,
    util::id::parse_id,
};

use self::destroy_params::DestroyParams;

/// The `destroy_multiple` parameters and their id cap.
mod destroy_params;

/// The 400 body of the legacy dismissal given no `id`.
const MISSING_ID: &str = "Missing parameter: id";
/// The 400 body of `read` given neither `id` nor `max_id`.
const MISSING_ID_OR_MAX_ID: &str = "Missing parameter: id or max_id";
/// The 400 body for a cursor that is not id text — the wording of
/// [`crate::pagination`] for the same failure.
const INVALID_ID: &str = "Invalid id";

/// The `/api/v1` notifications router, merged into [`crate::api::v1`]:
/// the flat listing and read and the dismissal routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/notifications", get(list))
        .route("/notifications/{id}", get(show))
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

/// `GET /api/v1/notifications` — one [`PageParams`] page of the caller's
/// notifications, newest row first, as the rendered entities with the
/// `Link` header keyed on the page's first and last row ids.
///
/// Pleroma's `index`, whose `for_user_query` scope (dismissed and
/// deactivated-actor rows out) [`notification::page_for_user`] carries;
/// `with_muted` decides what else hides there. The page's actors and
/// statuses are batched once into the render (see
/// [`render_notifications`]), which then applies the caller's active
/// filters in the `notifications` context: a rendered status a warning
/// filter matched carries its `filtered` array, and a notification whose
/// status an `irreversible` filter matched leaves the listing with it.
///
/// # Errors
///
/// [`Unauthorized`](crate::error::AppError::Unauthorized) /
/// [`Forbidden`](crate::error::AppError::Forbidden) from [`AuthUser`] and
/// the `read:notifications` check;
/// [`BadRequest`](crate::error::AppError::BadRequest) from [`PageParams`]
/// for a malformed cursor;
/// [`Unprocessable`](crate::error::AppError::Unprocessable) for a
/// `with_muted` that is not a boolean;
/// [`NotFound`](crate::error::AppError::NotFound) for an `account_id`
/// naming no readable account. Any database failure arrives through `?` as
/// [`Database`](crate::error::AppError::Database), and
/// [`set_link_header`] reports a `Link` value that is not header-safe as
/// [`Internal`](crate::error::AppError::Internal).
async fn list(
    State(state): State<AppState>,
    auth: AuthUser,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
    ApiParams(params): ApiParams<ListParams>,
) -> AppResult<(HeaderMap, Json<Vec<NotificationEntity>>)> {
    auth.require(READ_NOTIFICATIONS)?;
    let mut filters = params.type_filters();
    filters.account_id = account_filter(&state, params.account_id.as_deref()).await?;
    let rows = notification::page_for_user(
        &state.db,
        &FlatPageParams {
            user_id: auth.0.id,
            filters,
            with_muted: params.muted_visible(),
            page,
        },
    )
    .await?;
    debug!(
        count = rows.len(),
        user = %auth.0.id,
        "notifications listed"
    );
    let active = list_active(&state.db, auth.0.id).await?;
    let entities = render_notifications(&state, &auth.0, &rows, &active).await?;
    let mut headers = HeaderMap::new();
    set_link_header(
        &mut headers,
        keyset_link(
            &state.config.base_url,
            uri.path(),
            &NO_EXTRA_QUERY,
            &rows,
            |row| row.id,
        )
        .as_deref(),
    )?;
    Ok((headers, Json(entities)))
}

/// `GET /api/v1/notifications/{id}` — the caller's live notification `id`
/// as one rendered entity.
///
/// Pleroma's `show` asks only who owns the row; this keeps the listing's
/// scope, so the dismissed, the blocked, the muted and the
/// deactivated-actor's row read as the 404
/// the client already sees for an unknown id — and a malformed one, all
/// indistinguishable.
///
/// # Errors
///
/// The auth errors of [`list`];
/// [`NotFound`](crate::error::AppError::NotFound) as described.
async fn show(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<NotificationEntity>> {
    auth.require(READ_NOTIFICATIONS)?;
    let Some(id) = parse_id(&id) else {
        return Err(AppError::NotFound);
    };
    let row = notification::get_visible(&state.db, auth.0.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    debug!(
        notification = %id,
        user = %auth.0.id,
        "notification read"
    );
    // The v1 listing is the filtered read (the ticket's context map names
    // the *list*); a single notification read by id keeps its full render,
    // and `filters: &[]` is that pass's no-op spelling.
    let entities = render_notifications(&state, &auth.0, std::slice::from_ref(&row), &[]).await?;
    entities
        .into_iter()
        .next()
        .map(Json)
        .ok_or(AppError::NotFound)
}

/// `GET /api/v1/notifications`' non-cursor parameters ([`ApiParams`],
/// decision 10; the cursors and `limit` ride on [`PageParams`]).
#[derive(Debug, Deserialize)]
struct ListParams {
    /// `types[]`: keep only these notification types.
    #[serde(default)]
    types: Option<StringList>,
    /// `include_types[]`: the alias of `types[]`, consulted only when
    /// that is absent (Pleroma's `put_new("types", …)`).
    #[serde(default)]
    include_types: Option<StringList>,
    /// `exclude_types[]`: drop these notification types.
    #[serde(default)]
    exclude_types: Option<StringList>,
    /// `account_id`: only notifications caused by this account.
    #[serde(default)]
    account_id: Option<String>,
    /// `with_muted`: whether the caller's mutes (actor and thread) hide
    /// nothing; absent means `false`, Pleroma's `Map.get(:with_muted, false)`.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    with_muted: Option<bool>,
}

impl ListParams {
    /// The kept type names: `types[]` when it came, else the
    /// `include_types[]` alias, and neither means no restriction.
    fn kept_types(&self) -> Vec<&str> {
        names_of(self.types.as_ref().or(self.include_types.as_ref()))
    }

    /// The `types[]` / `exclude_types[]` half of the read filters; the
    /// account half arrives through [`account_filter`].
    fn type_filters(&self) -> NotificationFilters<'_> {
        NotificationFilters {
            types: self.kept_types(),
            exclude_types: names_of(self.exclude_types.as_ref()),
            account_id: None,
        }
    }

    /// Whether muted causes stay in the page.
    fn muted_visible(&self) -> bool {
        self.with_muted.unwrap_or_default()
    }
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
/// is likewise not an error. More than
/// [`MAX_BATCH_IDS`](crate::api::helpers::MAX_BATCH_IDS) distinct ids is
/// refused before anything is read or dismissed.
///
/// # Errors
///
/// The auth errors of [`clear`]; [`Unprocessable`](crate::error::AppError::Unprocessable)
/// (`Too many ids`) past the cap. Any database failure arrives through `?`
/// as [`Database`](crate::error::AppError::Database).
async fn destroy_multiple(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiParams(params): ApiParams<DestroyParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_NOTIFICATIONS)?;
    let ids = params.parsed_ids()?;
    let dismissed = notification::dismiss_ids(&state.db, auth.0.id, &ids).await?;
    debug!(
        dismissed,
        requested = ids.len(),
        user = %auth.0.id,
        "notifications dismissed in bulk"
    );
    Ok(empty_body())
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
    let entities = render_notifications(state, viewer, std::slice::from_ref(&row), &[]).await?;
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
    let entities = render_notifications(state, viewer, &marked, &[]).await?;
    Ok(ReadAnswer::Many(entities))
}

/// Renders `rows` as notification entities for `viewer`: every distinct
/// actor expanded once into an [`Account`] with the viewer's relationship
/// embedded, every distinct status the rows name rendered for the viewer
/// through the shared batched path (a status that is gone or hidden from
/// this reader simply leaves the entity's `status` key out), every chat
/// message the rows name read as the viewer's own reference and rendered
/// into the entity's `chat_message` (see [`rendered_chat_messages`]), and
/// the viewer's mutes read from the same relationship load that feeds the
/// account embeds. `filters` are the viewer's active phrase filters,
/// applied to every rendered status in the `notifications` context: a
/// warning match rides its entity's `filtered` array, and a status an
/// `irreversible` filter matches is hidden — and the notification naming
/// it leaves the listing with it. A row whose actor left no row (deleted
/// mid-request) drops out of the answer.
///
/// [`crate::api::streaming`] renders a hinted notification through this
/// same path, so the socket's entity and the endpoint's are built the
/// same way (the ticket's "as api/notifications.rs does").
///
/// # Errors
///
/// [`Database`](crate::error::AppError::Database) from the account,
/// relationship, status, chat-message and visibility reads.
pub(crate) async fn render_notifications(
    state: &AppState,
    viewer: &User,
    rows: &[NotificationRow],
    filters: &[Filter],
) -> AppResult<Vec<NotificationEntity>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let actor_ids = distinct(rows.iter().map(|row| row.from_account_id));
    let status_ids = distinct(rows.iter().filter_map(|row| row.status_id));
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
    let statuses = visible_statuses(state, Some(viewer), &status_ids, filters).await?;
    let chat_messages = rendered_chat_messages(state, viewer, rows).await?;
    let mut entities = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(actor) = users.get(&row.from_account_id) else {
            continue;
        };
        if row
            .status_id
            .is_some_and(|id| statuses.hidden_by_filter.contains(&id))
        {
            continue;
        }
        let status = row
            .status_id
            .and_then(|id| statuses.rendered.get(&id).cloned());
        let chat_message = row
            .chat_message_id
            .and_then(|id| chat_messages.get(&id).cloned());
        entities.push(NotificationEntity::from_row(
            row,
            Account::from_user_for(Some(viewer), actor, &state.config, Some(&relationships)),
            status,
            chat_message,
            relationships
                .get(&row.from_account_id)
                .is_some_and(|found| found.muting.0),
        ));
    }
    Ok(entities)
}

/// The notification render's chat half: every chat message the rows name,
/// read as the viewer's own reference and rendered through the shared
/// chat-message path (see [`chat_message_entity`]) — serialized and keyed
/// by the message row's id (the entity's own `id` is the *reference* id,
/// the viewer's own, so the map keys on the id the notification stores).
/// A message the viewer holds no reference to — they took it out of their
/// chat — is not in the map, and its notification renders without the
/// `chat_message` key.
///
/// [`ChatMessage`]: crate::entities::chat::ChatMessage
///
/// # Errors
///
/// [`Database`](crate::error::AppError::Database) from the reference and
/// attachment reads;
/// [`Internal`](crate::error::AppError::Internal) if an entity would not
/// serialize.
async fn rendered_chat_messages(
    state: &AppState,
    viewer: &User,
    rows: &[NotificationRow],
) -> AppResult<HashMap<Uuid, Value>> {
    let mut rendered = HashMap::new();
    for message_id in distinct(rows.iter().filter_map(|row| row.chat_message_id)) {
        let Some(value) = chat_message_entity(state, viewer.id, message_id).await? else {
            continue;
        };
        let _first = rendered.insert(message_id, value);
    }
    Ok(rendered)
}

/// The viewer's own [`ChatMessage`] entity for message `message_id`: their
/// reference read through [`chat::message_reference_for_user`] (Pleroma's
/// `MessageReference.for_chat_and_object` of the reader's chat), rendered
/// with its attachment row when it carries one, and serialized; `None` when
/// the viewer holds no reference to the message.
///
/// [`ChatMessage`]: crate::entities::chat::ChatMessage
///
/// # Errors
///
/// [`Database`](crate::error::AppError::Database) from the reference and
/// media reads; [`Internal`](crate::error::AppError::Internal) if the entity
/// would not serialize.
async fn chat_message_entity(
    state: &AppState,
    viewer_id: Uuid,
    message_id: Uuid,
) -> AppResult<Option<Value>> {
    let Some(reference) =
        chat::message_reference_for_user(&state.db, viewer_id, message_id).await?
    else {
        return Ok(None);
    };
    let entity = message_of(state, &reference).await?;
    let value = serde_json::to_value(entity).map_err(|err| {
        AppError::Internal(format!("chat message entity failed to serialize: {err}"))
    })?;
    Ok(Some(value))
}

/// The notification render's status half: every visible row rendered,
/// filtered for the `notifications` context and serialized, keyed by its
/// row id (the rendered [`Status`] entity does not duplicate cheaply, and
/// several notifications can name one status, so the map holds its JSON
/// shape), beside the ids the irreversible filters hid — the rows whose
/// notifications leave the listing rather than render without a status.
///
/// [`Status`]: crate::entities::status::Status
#[derive(Debug)]
struct RenderedStatuses {
    /// The surviving rendered entities, serialized, keyed by row id.
    rendered: HashMap<Uuid, Value>,
    /// The ids the irreversible filter matches removed from `rendered`.
    hidden_by_filter: Vec<Uuid>,
}

/// The distinct status ids of `ids`, rendered as seen by `viewer`, run
/// through [`apply_filters`] in the `notifications` context, serialized,
/// and keyed by their row ids: rows that name no row, that a delete took,
/// or that [`status_visibility`] hides from this reader are not in the map
/// (see [`RenderedStatuses`] for what the map and the hidden list carry).
///
/// # Errors
///
/// [`Database`](crate::error::AppError::Database) from the status and
/// visibility reads;
/// [`Internal`](crate::error::AppError::Internal) if a rendered entity
/// would not serialize.
async fn visible_statuses(
    state: &AppState,
    viewer: AccountViewer<'_>,
    ids: &[Uuid],
    filters: &[Filter],
) -> AppResult<RenderedStatuses> {
    if ids.is_empty() {
        return Ok(RenderedStatuses {
            rendered: HashMap::new(),
            hidden_by_filter: Vec::new(),
        });
    }
    let rows = status::get_many(&state.db, ids).await?;
    let mut visible: Vec<StatusRow> = Vec::new();
    for row in rows {
        if status_visibility::can_view(&state.db, &row, viewer).await? {
            visible.push(row);
        }
    }
    let page = render_statuses(state, ViewerMode::User(viewer), &visible).await?;
    let mut all_ids: Vec<Uuid> = page
        .iter()
        .filter_map(|entity| parse_id(&entity.id))
        .collect();
    let mut rendered = HashMap::new();
    let kept = apply_filters(
        page,
        filters,
        FilterContext::Notifications,
        viewer.map(|user| user.id),
    );
    for entity in kept {
        let Some(id) = parse_id(&entity.id) else {
            continue;
        };
        let value = serde_json::to_value(entity).map_err(|err| {
            AppError::Internal(format!("status entity failed to serialize: {err}"))
        })?;
        let _first = rendered.insert(id, value);
    }
    // What the render produced but the filtered page did not keep is what
    // an `irreversible` match removed — invisible rows never entered the
    // render at all, so the two hides stay distinguishable.
    all_ids.retain(|id| !rendered.contains_key(id));
    Ok(RenderedStatuses {
        rendered,
        hidden_by_filter: all_ids,
    })
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

    #[test]
    fn the_listing_types_prefer_the_name_and_fall_back_to_the_alias() {
        let both: ListParams = params(json!({
            "types": ["favourite", "mention"],
            "include_types": "follow",
            "exclude_types": ["reblog"],
        }));
        let filters = both.type_filters();
        assert_eq!(
            filters.types,
            ["favourite", "mention"],
            "`types[]` wins when both arrive, Pleroma's put_new rule"
        );
        assert_eq!(
            filters.exclude_types,
            ["reblog"],
            "the exclusion is its own list"
        );

        let aliased: ListParams = params(json!({"include_types": "follow"}));
        assert_eq!(
            aliased.type_filters().types,
            ["follow"],
            "the alias fills in when `types[]` is absent"
        );

        let neither: ListParams = params(json!({}));
        let open = neither.type_filters();
        assert!(
            open.types.is_empty() && open.exclude_types.is_empty(),
            "absent names keep everything open"
        );
        assert!(!neither.muted_visible(), "with_muted defaults to false");
    }

    #[test]
    fn the_listing_reads_with_muted_in_its_client_spellings() {
        for spelling in ["true", "1", "yes"] {
            let asked: ListParams = params(json!({"with_muted": spelling}));
            assert!(
                asked.muted_visible(),
                "`with_muted={spelling}` asks to keep the muted rows"
            );
        }
        for spelling in ["false", "0"] {
            let asked: ListParams = params(json!({"with_muted": spelling}));
            assert!(!asked.muted_visible(), "`with_muted={spelling}` hides them");
        }
        for spelling in ["maybe", "2"] {
            let err = crate::extract::into_params::<ListParams>(json!({"with_muted": spelling}))
                .expect_err(&format!("`{spelling}` is no boolean"));
            assert!(
                matches!(err, AppError::Unprocessable(_)),
                "which is a 422, not {err}"
            );
        }
    }
}
