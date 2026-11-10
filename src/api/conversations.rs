//! Direct-message conversations over HTTP: the Mastodon v1 listing and
//! its two writes, and Pleroma's five conversation routes, on the rows of
//! [`crate::models::conversation`] (decision 3).
//!
//! The reference is Pleroma's `ConversationController`
//! (`lib/pleroma/web/mastodon_api/controllers/conversation_controller.ex`,
//! the `index`, `mark_as_read` and `delete` actions) and its
//! `PleromaAPI.ConversationController`
//! (`lib/pleroma/web/pleroma_api/controllers/conversation_controller.ex`,
//! the `show`, `statuses`, `update` and `mark_as_read` actions), with the
//! entity from `ConversationView.render("participation.json")`
//! (`lib/pleroma/web/mastodon_api/views/conversation_view.ex`): every
//! route answers with the [`Conversation`] entity — the participation's
//! own id, `unread` the row's read flag inverted, `accounts` the
//! participation's recipient list expanded as the viewer sees them (the
//! viewer is never on it), and `last_status` the thread's newest status
//! rendered through the shared status batch, `null` when the participation
//! points at nothing the viewer may render.
//!
//! - `GET /api/v1/conversations` — one [`PageParams`] page (default 20,
//!   at most 40) of the caller's participations, newest activity first,
//!   under a `Link` header keyed on the participation ids of the page.
//!   `recipients[]` is Pleroma's `restrict_recipients`, restated over this
//!   schema's recipient lists: every named account must be among the
//!   participation's addressees; ids that are not id text match nothing
//!   and are skipped, as in the other id-array readers here.
//! - `POST /api/v1/conversations/{id}/read` — flags the caller's
//!   participation read and answers its entity; a malformed, unknown or
//!   foreign id is the shared 404 `{"error":"Record not found"}`. A repeat
//!   mark answers the same entity.
//! - `DELETE /api/v1/conversations/{id}` — withdraws the caller's
//!   participation (the thread's statuses are untouched: the other
//!   participants keep listing it, Pleroma's `Participation.delete`) and
//!   answers `{}`; the same misses are the same 404.
//! - `GET /api/v1/pleroma/conversations/{id}` — Pleroma's `show`: one
//!   owned participation as the entity; every miss is the 404
//!   `{"error":"Unknown conversation id"}` that controller renders.
//! - `GET /api/v1/pleroma/conversations/{id}/statuses` — Pleroma's
//!   `statuses`: the thread's live `direct` statuses the caller may see
//!   ([`status_visibility`] applied per row), newest first, one
//!   [`PageParams`] page under a `Link` header keyed on the status ids of
//!   the page; the same 404 for a participation that is not the caller's.
//! - `PATCH /api/v1/pleroma/conversations/{id}` — Pleroma's `update`: the
//!   body's `recipients[]` (account ids, a JSON array or `recipients[]=`
//!   form keys, decision 10) becomes the participation's whole addressee
//!   list. The caller is excluded automatically (Pleroma stores the owner
//!   on the list, this schema never does); an id that is not id text or
//!   names no stored account is Pleroma's refused recipient, which the
//!   ticket folds into the same 404. An empty list clears it. The
//!   rewritten entity answers.
//! - `POST /api/v1/pleroma/conversations/read` — Pleroma's
//!   `mark_as_read`: flags every unread participation of the caller read
//!   at once and answers the entities of exactly the rows that flipped,
//!   newest first — the empty array when nothing was unread.
//!
//! Gates, in Pleroma's order: a user token ([`AuthUser`]) granting
//! `read:statuses` on the three reads and `write:conversations` on the
//! four writes (the `OAuthScopesPlug` clauses of the two controllers).
//! Ownership is the id itself: each account's participations carry their
//! own ids and never share them, so [`crate::models::conversation`]'s
//! owner-keyed reads answer a foreign id exactly like an unknown one.
//!
//! [`status_visibility`]: crate::models::status_visibility
//! [`Conversation`]: crate::entities::conversation::Conversation

use std::collections::{HashMap, HashSet};

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
    routing::{delete, get, post},
};
use serde::Deserialize;
use serde_json::{Map, Value};
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::notifications_v2::{NO_EXTRA_QUERY, StringList, names_of},
    auth::AuthUser,
    entities::{
        account::{Account, AccountViewer},
        conversation::Conversation,
        relationship::{Relationship, RelationshipMap},
        status::Status as StatusEntity,
    },
    error::{AppError, AppResult},
    extract::{ApiBody, ApiParams},
    models::{
        conversation::{self, Participation},
        status::{self, Status as StatusRow},
        status_visibility,
        user::{self, User},
    },
    oauth::scopes::{READ_STATUSES, WRITE_CONVERSATIONS},
    pagination::{PageParams, link_header, set_link_header},
    services::render_status::render_statuses,
    state::AppState,
    util::id::parse_id,
};

/// The 404 body Pleroma's `PleromaAPI.ConversationController` renders for
/// every miss on its routes (`json(%{"error" => "Unknown conversation id"})`).
const UNKNOWN_CONVERSATION: &str = "Unknown conversation id";

/// The `/api/v1` conversations router, merged into [`crate::api::v1`]:
/// the Mastodon listing, the read and the withdrawal.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/conversations", get(list))
        .route("/conversations/{id}/read", post(mark_read))
        .route("/conversations/{id}", delete(remove))
}

/// The `/api/v1/pleroma/conversations` router, nested into
/// [`crate::api::v1`]: Pleroma's show, statuses, recipient rewrite and
/// read-all.
pub fn pleroma_router() -> Router<AppState> {
    Router::new()
        .route("/read", post(mark_all_read))
        .route("/{id}", get(show).patch(update))
        .route("/{id}/statuses", get(list_statuses))
}

/// The `{}` answer of the withdrawal route (Pleroma's `json(conn, %{})`).
fn empty_body() -> Json<Value> {
    Json(Value::Object(Map::new()))
}

/// The miss of Pleroma's `PleromaAPI` conversation controller: the 404
/// naming the conversation it could not find, wording included.
fn unknown_conversation() -> AppError {
    AppError::NotFoundMessage(UNKNOWN_CONVERSATION.to_owned())
}

/// `GET /api/v1/conversations` — one [`PageParams`] page of the caller's
/// participations, newest activity first, as the [`Conversation`] entities
/// with the `Link` header keyed on the page's participation ids.
///
/// Pleroma's `index`; `recipients[]` restricts the page in the database
/// (see [`conversation::page_for_user`]) so the cap counts the kept rows.
///
/// # Errors
///
/// [`Unauthorized`](crate::error::AppError::Unauthorized) /
/// [`Forbidden`](crate::error::AppError::Forbidden) from [`AuthUser`] and
/// the `read:statuses` check;
/// [`BadRequest`](crate::error::AppError::BadRequest) from [`PageParams`]
/// for a malformed cursor. Any database failure arrives through `?` as
/// [`Database`](crate::error::AppError::Database), and [`set_link_header`]
/// reports a `Link` value that is not header-safe as
/// [`Internal`](crate::error::AppError::Internal).
async fn list(
    State(state): State<AppState>,
    auth: AuthUser,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
    ApiParams(params): ApiParams<ListParams>,
) -> AppResult<(HeaderMap, Json<Vec<Conversation>>)> {
    auth.require(READ_STATUSES)?;
    let recipients = params.recipient_ids();
    let rows = conversation::page_for_user(&state.db, auth.0.id, page, &recipients).await?;
    debug!(
        count = rows.len(),
        user = %auth.0.id,
        "conversations listed"
    );
    let entities = conversation_entities(&state, &auth.0, &rows).await?;
    let mut headers = HeaderMap::new();
    set_link_header(
        &mut headers,
        participation_link(&state.config.base_url, uri.path(), &rows).as_deref(),
    )?;
    Ok((headers, Json(entities)))
}

/// `GET /api/v1/conversations`' non-cursor parameters ([`ApiParams`],
/// decision 10; the cursors and `limit` ride on [`PageParams`]).
#[derive(Debug, Deserialize)]
struct ListParams {
    /// `recipients[]`: keep only the conversations addressing every one
    /// of these accounts.
    #[serde(default)]
    recipients: Option<StringList>,
}

impl ListParams {
    /// The requested recipient ids, each named once, in the order sent;
    /// values that are not id text are skipped — they can name no
    /// addressee anyway, and a filter that matches nothing narrows to the
    /// empty page on its own (the other id-array readers here skip
    /// likewise).
    fn recipient_ids(&self) -> Vec<Uuid> {
        recipient_ids(names_of(self.recipients.as_ref()))
    }
}

/// The parsed, deduplicated ids of `values`, in the order sent, skipping
/// the text that is no id.
fn recipient_ids<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<Uuid> {
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

/// `POST /api/v1/conversations/{id}/read` — flag the caller's
/// participation read and answer its entity; Pleroma's `mark_as_read`,
/// whose foreign- or missing-row refusal is the shared 404. A repeat mark
/// flags nothing new and answers the same entity.
///
/// # Errors
///
/// The auth errors of [`list`] with the `write:conversations` scope;
/// [`NotFound`](crate::error::AppError::NotFound) for an id that is
/// malformed, unknown or somebody else's.
async fn mark_read(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Conversation>> {
    auth.require(WRITE_CONVERSATIONS)?;
    let Some(id) = parse_id(&id) else {
        return Err(AppError::NotFound);
    };
    let row = conversation::mark_read(&state.db, auth.0.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    debug!(
        conversation = %id,
        user = %auth.0.id,
        "conversation marked read"
    );
    one_entity(&state, &auth.0, &row).await
}

/// `DELETE /api/v1/conversations/{id}` — withdraw the caller's
/// participation and answer `{}` (Pleroma's `delete`). Only this side of
/// the thread goes: the statuses stay stored and the other participants
/// keep their own participations and recipient rows.
///
/// # Errors
///
/// The auth errors of [`mark_read`];
/// [`NotFound`](crate::error::AppError::NotFound) for an id that is
/// malformed, unknown or somebody else's.
async fn remove(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_CONVERSATIONS)?;
    let Some(id) = parse_id(&id) else {
        return Err(AppError::NotFound);
    };
    if !conversation::delete_participation(&state.db, auth.0.id, id).await? {
        return Err(AppError::NotFound);
    }
    debug!(
        conversation = %id,
        user = %auth.0.id,
        "participation withdrawn"
    );
    Ok(empty_body())
}

/// `GET /api/v1/pleroma/conversations/{id}` — Pleroma's `show`: the
/// caller's participation `id` as one entity.
///
/// # Errors
///
/// The auth errors of [`list`];
/// [`NotFoundMessage`](crate::error::AppError::NotFoundMessage) naming the
/// conversation for a malformed, unknown or foreign id.
async fn show(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Conversation>> {
    auth.require(READ_STATUSES)?;
    let row = owned_participation(&state, auth.0.id, &id).await?;
    one_entity(&state, &auth.0, &row).await
}

/// `GET /api/v1/pleroma/conversations/{id}/statuses` — Pleroma's
/// `statuses`: the thread's live `direct` statuses the caller may see,
/// newest first, one [`PageParams`] page under a `Link` header keyed on
/// the status ids of the page.
///
/// The participation is the whole gate — Pleroma's controller asks only
/// whether the row belongs to the user — and per row the answer is the
/// visibility model's ([`status_visibility`] rule 6: a `direct` post
/// belongs to its author and the accounts it @-mentions).
///
/// # Errors
///
/// The auth errors of [`show`] and [`PageParams`]' bad cursor; the same
/// 404 [`show`] renders.
async fn list_statuses(
    State(state): State<AppState>,
    auth: AuthUser,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
) -> AppResult<(HeaderMap, Json<Vec<StatusEntity>>)> {
    auth.require(READ_STATUSES)?;
    let row = owned_participation(&state, auth.0.id, &id).await?;
    let fetched = conversation::page_direct_statuses(&state.db, row.conversation_id, page).await?;
    let mut visible: Vec<StatusRow> = Vec::new();
    for status in &fetched {
        if status_visibility::can_view(&state.db, status, Some(&auth.0)).await? {
            visible.push(status.clone());
        }
    }
    debug!(
        conversation = %row.id,
        count = visible.len(),
        user = %auth.0.id,
        "conversation statuses listed"
    );
    let entities = render_statuses(&state, Some(&auth.0), &visible).await?;
    let mut headers = HeaderMap::new();
    set_link_header(
        &mut headers,
        status_link(&state.config.base_url, uri.path(), &entities).as_deref(),
    )?;
    Ok((headers, Json(entities)))
}

/// The body of `PATCH /api/v1/pleroma/conversations/{id}`: `recipients`,
/// required — Pleroma's clause pattern will not fire without it.
#[derive(Debug, Deserialize)]
struct RecipientsParams {
    /// `recipients[]`: the account ids that become the whole addressee
    /// list.
    recipients: StringList,
}

/// `PATCH /api/v1/pleroma/conversations/{id}` — Pleroma's `update`:
/// replace the participation's recipient list with the body's accounts.
/// The caller is excluded from the rewrite automatically (Pleroma keeps
/// the owner on its list; this schema's lists never hold them); an id that
/// is not id text or names no stored account is Pleroma's refused
/// recipient, the ticket's 404. The rewritten entity — same participation,
/// new `accounts` — answers.
///
/// # Errors
///
/// The auth errors of [`mark_read`]; the same 404 [`show`] renders for an
/// unreadable participation or a refused recipient;
/// [`Unprocessable`](crate::error::AppError::Unprocessable) from the
/// extractor for a body without `recipients`.
async fn update(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(body): ApiBody<RecipientsParams>,
) -> AppResult<Json<Conversation>> {
    auth.require(WRITE_CONVERSATIONS)?;
    let row = owned_participation(&state, auth.0.id, &id).await?;
    let recipients = kept_recipients(requested_recipients(&body.recipients)?, auth.0.id);
    if !all_are_accounts(&state, &recipients).await? {
        return Err(unknown_conversation());
    }
    conversation::set_recipients(&state.db, row.id, &recipients).await?;
    debug!(
        conversation = %row.id,
        recipients = recipients.len(),
        user = %auth.0.id,
        "conversation recipients rewritten"
    );
    one_entity(&state, &auth.0, &row).await
}

/// The rewrite's requested ids, each named once, in the order sent, with
/// the skips disallowed — a value that is not id text is Pleroma's
/// unknown recipient (the ticket's 404), not a silent drop that would
/// quietly shorten the caller's roster.
///
/// # Errors
///
/// [`NotFoundMessage`](crate::error::AppError::NotFoundMessage) when any
/// value is not id text.
fn requested_recipients(list: &StringList) -> AppResult<Vec<Uuid>> {
    let mut ids: Vec<Uuid> = Vec::new();
    for name in list.names() {
        let Some(id) = parse_id(name) else {
            return Err(unknown_conversation());
        };
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// The addressee list the rewrite stores: the requested ids with the
/// caller filtered out (the viewer is excluded from the rewrite
/// automatically; Pleroma's `set_recipients` re-adds the owner to its
/// own list, this schema's lists never hold them).
fn kept_recipients(requested: Vec<Uuid>, viewer: Uuid) -> Vec<Uuid> {
    requested.into_iter().filter(|id| *id != viewer).collect()
}

/// Whether every asked id names a stored account: [`user::get_many`]
/// answers one row per id that exists, so a short count is Pleroma's
/// `set_recipients` finding no user for one of the requested ids. An
/// empty ask needs no read — clearing the list is not a miss.
///
/// # Errors
///
/// [`Database`](crate::error::AppError::Database) from the account read.
async fn all_are_accounts(state: &AppState, ids: &[Uuid]) -> AppResult<bool> {
    if ids.is_empty() {
        return Ok(true);
    }
    let found = user::get_many(&state.db, ids).await?;
    Ok(found.len() == ids.len())
}

/// `POST /api/v1/pleroma/conversations/read` — Pleroma's `mark_as_read`:
/// flag every unread participation of the caller read at once, and answer
/// the entities of exactly the rows that flipped, newest first. Nothing
/// unread answers the empty array and writes nothing.
///
/// # Errors
///
/// The auth errors of [`mark_read`].
async fn mark_all_read(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<Vec<Conversation>>> {
    auth.require(WRITE_CONVERSATIONS)?;
    let mut flipped = conversation::mark_all_read(&state.db, auth.0.id).await?;
    flipped.sort_unstable_by_key(|row| std::cmp::Reverse(row.id));
    debug!(
        count = flipped.len(),
        user = %auth.0.id,
        "conversations marked read"
    );
    Ok(Json(
        conversation_entities(&state, &auth.0, &flipped).await?,
    ))
}

/// The participation `id_text` of `user_id`, or the controller's 404: a
/// malformed id, an unknown one and a row somebody else owns are one
/// indistinguishable miss, which is what makes the id a capability.
///
/// # Errors
///
/// [`NotFoundMessage`](crate::error::AppError::NotFoundMessage) as
/// described.
async fn owned_participation(
    state: &AppState,
    user_id: Uuid,
    id_text: &str,
) -> AppResult<Participation> {
    let Some(id) = parse_id(id_text) else {
        return Err(unknown_conversation());
    };
    conversation::get_participation(&state.db, user_id, id)
        .await?
        .ok_or_else(unknown_conversation)
}

/// The [`Conversation`] entity of the single `row`, through the page
/// renderer.
///
/// # Errors
///
/// As [`conversation_entities`].
async fn one_entity(
    state: &AppState,
    viewer: &User,
    row: &Participation,
) -> AppResult<Json<Conversation>> {
    let mut entities = conversation_entities(state, viewer, std::slice::from_ref(row)).await?;
    entities
        .pop()
        .map(Json)
        .ok_or_else(|| AppError::Internal("a rendered participation lost its entity".to_owned()))
}

/// Renders `rows` as [`Conversation`] entities for `viewer`: every
/// participation's addressee ids in one batched read, those accounts
/// expanded once each with the viewer's relationship embedded, and the
/// pages' `last_status_id`s rendered through the shared status batch —
/// rows the viewer may not see or that no longer exist simply leave the
/// map, and their conversation's `last_status` goes `null`.
///
/// # Errors
///
/// [`Database`](crate::error::AppError::Database) from the recipient,
/// account, relationship, status and visibility reads;
/// [`Internal`](crate::error::AppError::Internal) if a rendered status
/// entity would not serialize.
async fn conversation_entities(
    state: &AppState,
    viewer: &User,
    rows: &[Participation],
) -> AppResult<Vec<Conversation>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let lists = conversation::recipients_map(&state.db, &ids).await?;
    let account_ids = distinct(lists.values().flatten().copied());
    let users = user::get_many(&state.db, &account_ids)
        .await?
        .into_iter()
        .map(|found| (found.id, found))
        .collect::<HashMap<Uuid, User>>();
    let relationships = Relationship::load_many(&state.db, viewer.id, &account_ids)
        .await?
        .into_iter()
        .zip(account_ids)
        .map(|(relationship, target)| (target, relationship))
        .collect::<RelationshipMap>();
    let last_status_ids = distinct(rows.iter().filter_map(|row| row.last_status_id));
    let last_statuses = visible_statuses(state, Some(viewer), &last_status_ids).await?;
    let entities = rows
        .iter()
        .map(|row| {
            let accounts = lists
                .get(&row.id)
                .map_or(&[][..], Vec::as_slice)
                .iter()
                .filter_map(|id| users.get(id))
                .map(|account| {
                    Account::from_user_for(
                        Some(viewer),
                        account,
                        &state.config,
                        Some(&relationships),
                    )
                })
                .collect();
            let last_status = row
                .last_status_id
                .and_then(|id| last_statuses.get(&id).cloned());
            Conversation::from_row(row, accounts, last_status)
        })
        .collect();
    Ok(entities)
}

/// The distinct ids of `values`, in order of first appearance.
fn distinct(values: impl Iterator<Item = Uuid>) -> Vec<Uuid> {
    let mut seen = HashSet::new();
    values.filter(|id| seen.insert(*id)).collect()
}

/// The status rows `ids` names, as `viewer` may see them, rendered and
/// keyed by their row ids — the batch the conversations' `last_status`
/// embeds come from (gone, tombstoned and unseen ids are simply absent,
/// and their conversation renders `last_status: null`; the notifications
/// listing keeps the same shape for the same reason).
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

/// The `Link` header value for one page of participations: [`link_header`]
/// over the first and last row's participation id — the ids the keyset
/// walks — and `None` for an empty page.
fn participation_link(base_url: &str, path: &str, rows: &[Participation]) -> Option<String> {
    let newest = rows.first()?.id.to_string();
    let oldest = rows.last()?.id.to_string();
    link_header(base_url, path, &NO_EXTRA_QUERY, &newest, &oldest)
}

/// The `Link` header value for one page of rendered statuses: the same
/// [`link_header`] over the entities' ids.
fn status_link(base_url: &str, path: &str, rows: &[StatusEntity]) -> Option<String> {
    let newest = rows.first()?.id.clone();
    let oldest = rows.last()?.id.clone();
    link_header(base_url, path, &NO_EXTRA_QUERY, &newest, &oldest)
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
    fn the_listing_reads_recipients_as_ids_each_named_once() {
        let first = "01934567-89ab-7cde-f012-3456789abcde";
        let second = "01934567-89ab-7cde-f012-3456789abcdf";
        let asked: ListParams = params(json!({"recipients": [first, second, first, "junk"]}));
        assert_eq!(
            asked.recipient_ids(),
            vec![
                first.parse::<Uuid>().expect("fixture id"),
                second.parse::<Uuid>().expect("fixture id"),
            ],
            "the json array, deduplicated, non-ids skipped"
        );
        let lone: ListParams = params(json!({"recipients": first}));
        assert_eq!(
            lone.recipient_ids().len(),
            1,
            "and the lone `recipients=` value"
        );
        let absent: ListParams = params(json!({}));
        assert!(
            absent.recipient_ids().is_empty(),
            "absent asks for the open listing"
        );
    }

    #[test]
    fn the_rewrite_refuses_values_that_are_not_ids() {
        let first = "01934567-89ab-7cde-f012-3456789abcde";
        let second = "01934567-89ab-7cde-f012-3456789abcdf";
        let body: RecipientsParams = params(json!({"recipients": [first, second, first]}));
        assert_eq!(
            requested_recipients(&body.recipients).expect("every value is id text"),
            vec![
                first.parse::<Uuid>().expect("fixture id"),
                second.parse::<Uuid>().expect("fixture id"),
            ],
            "the list arrives deduplicated, in order"
        );
        let junk: RecipientsParams = params(json!({"recipients": ["not-an-id"]}));
        let err = requested_recipients(&junk.recipients)
            .expect_err("junk names no account and must not silently drop");
        assert_eq!(err.to_string(), UNKNOWN_CONVERSATION, "the ticket's 404");
        assert!(
            matches!(err, AppError::NotFoundMessage(_)),
            "which is a 404, not {err}"
        );
    }

    #[test]
    fn the_rewrite_excludes_the_viewer_from_the_list() {
        let viewer = Uuid::now_v7();
        let other = Uuid::now_v7();
        assert_eq!(
            kept_recipients(vec![viewer, other], viewer),
            vec![other],
            "the caller is dropped automatically"
        );
        assert!(
            kept_recipients(vec![viewer], viewer).is_empty(),
            "a list of just the viewer ends up empty — the clearing ask"
        );
    }
}
