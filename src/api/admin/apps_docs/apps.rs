//! The admin OAuth app routes: validation, the app body and the CRUD
//! handlers.

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tracing::debug;

use super::{AREA_APPS, CLASS_WRITE};
use crate::{
    api::apps::{RedirectUris, normalize_redirect_uris},
    auth::admin::AdminUser,
    entities::admin::{AdminOauthApp, AdminOauthAppPage},
    error::{AppError, AppResult},
    extract::{ApiBody, flex},
    models::oauth_app::{self, AdminNewApp, AppChanges},
    oauth::scopes::Scopes,
    state::AppState,
    util::id::parse_id,
};

/// The refusal Ecto renders for a blank required field, the message the
/// ticket pins.
const CANNOT_BE_BLANK: &str = "can't be blank";
/// What an app handler can fail with: the project error as-is, or
/// Pleroma's `CastAndValidate` 400 naming the refused fields.
#[derive(Debug)]
pub(super) enum AppsError {
    /// An ordinary [`AppError`], rendered by its own `IntoResponse`.
    App(AppError),
    /// The 400 `{"errors":{"<field>":["<message>"]}}` the ticket pins for
    /// validation refusals; several fields at once when one body refused
    /// on several, as an Ecto changeset reports all its errors together.
    Invalid(Vec<(&'static str, String)>),
}

/// An [`AppsError`] is a response: the wrapped [`AppError`] renders
/// itself, the invalid arm the pinned `{"errors": …}` 400.
impl IntoResponse for AppsError {
    fn into_response(self) -> Response {
        match self {
            Self::App(err) => err.into_response(),
            Self::Invalid(refusals) => {
                let mut errors = Map::new();
                for (field, message) in refusals {
                    let entry = Value::Array(vec![Value::String(message)]);
                    let replaced = errors.insert(field.to_owned(), entry);
                    drop(replaced);
                }
                let body = json!({ "errors": errors });
                (StatusCode::BAD_REQUEST, Json(body)).into_response()
            }
        }
    }
}

/// Every [`AppError`] the extractors, scope gates and models raise
/// travels through unchanged.
impl From<AppError> for AppsError {
    fn from(err: AppError) -> Self {
        Self::App(err)
    }
}

/// The handlers' result with [`AppsError`] as the error half.
pub(super) type AppsResult<T> = Result<T, AppsError>;

/// The body of `POST /oauth_app` and `PATCH /oauth_app/{id}`: every
/// mutable field of the entity, read as an `Option` so `create` can name
/// its own missing-field refusals and `update` can tell absent from
/// present. `redirect_uris` takes the two spellings client registration
/// takes; `trusted` deserializes through [`flex::opt_bool`] so a
/// form-encoded `"true"` counts.
#[derive(Debug, Deserialize)]
pub(super) struct AppBody {
    /// The application name; required on create.
    #[serde(default)]
    name: Option<String>,
    /// The redirect URIs; required on create.
    #[serde(default)]
    redirect_uris: Option<RedirectUris>,
    /// The granted scopes; absent means `read` on create, kept-as-stored
    /// on update.
    #[serde(default)]
    scopes: Option<ScopeInput>,
    /// The homepage; JSON `null` reads as absent.
    #[serde(default)]
    website: Option<String>,
    /// The consent-skip flag; absent means untrusted on create.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    trusted: Option<bool>,
}

/// `scopes` as admins send it: one string (space- or `+`-separated) or a
/// list (a JSON array, or form `scopes[]=…` keys).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ScopeInput {
    /// One scope string, parsed as [`Scopes::parse`] parses it.
    Single(String),
    /// One name per element, joined with spaces before parsing.
    List(Vec<String>),
}

impl ScopeInput {
    /// The scope set behind the input: a list joins its elements with
    /// spaces, and [`Scopes::parse`] then splits everything back on
    /// spaces and `+` signs, dropping blanks and duplicates.
    fn parse(self) -> Scopes {
        match self {
            Self::Single(text) => Scopes::parse(&text),
            Self::List(names) => Scopes::parse(&names.join(" ")),
        }
    }
}

/// The validated, storage-ready form of an [`AppBody`]: every field the
/// handlers write, `None` where nothing was sent (and, on create, where
/// the field has a default instead).
#[derive(Debug)]
struct Prepared {
    /// The trimmed application name.
    name: Option<String>,
    /// The normalized redirect-URI string.
    redirect_uris: Option<String>,
    /// The stored scope string: the default `read` on a create that sent
    /// none, `None` on an update that sent none.
    scopes: Option<String>,
    /// The homepage as sent.
    website: Option<String>,
    /// The trusted flag as sent.
    trusted: Option<bool>,
}

/// Validates and normalizes a body: the trim/blank gate on `name`, the
/// registration normalization on `redirect_uris`, and the scope parse
/// and [`Scopes::validate`] check — every refusal joining one
/// `{"errors": …}` answer. `on_create` decides whether an absent field
/// is a refusal or simply nothing to write.
///
/// # Errors
///
/// [`AppsError::Invalid`] with the `can't be blank` and `invalid
/// scopes` refusals the body earns.
fn prepare(body: AppBody, on_create: bool) -> Result<Prepared, AppsError> {
    let name_sent = body.name.is_some();
    let uris_sent = body.redirect_uris.is_some();
    let name = body
        .name
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty());
    let redirect_uris = body
        .redirect_uris
        .map(normalize_redirect_uris)
        .filter(|text| !text.trim().is_empty());
    let mut refusals: Vec<(&'static str, String)> = Vec::new();
    let scopes = match body.scopes {
        None if on_create => Some(Scopes::default().to_string()),
        None => None,
        Some(input) => {
            let parsed = input.parse();
            match parsed.validate() {
                Ok(()) => Some(parsed.to_string()),
                Err(reason) => {
                    refusals.push(("scopes", format!("invalid scopes: {reason}")));
                    None
                }
            }
        }
    };
    if name.is_none() && (on_create || name_sent) {
        refusals.push(("name", CANNOT_BE_BLANK.to_owned()));
    }
    if redirect_uris.is_none() && (on_create || uris_sent) {
        refusals.push(("redirect_uris", CANNOT_BE_BLANK.to_owned()));
    }
    if !refusals.is_empty() {
        return Err(AppsError::Invalid(refusals));
    }
    Ok(Prepared {
        name,
        redirect_uris,
        scopes,
        website: body.website,
        trusted: body.trusted,
    })
}

/// `GET /api/v1/pleroma/admin/oauth_app` — the filtered, windowed app
/// listing as `{apps, count, page_size}`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate (the listing serializes every
/// `client_secret`, so a read grant never opens it); [`AppError::BadRequest`]
/// for a `trusted` that is no boolean; [`AppError::Database`] from the two
/// reads.
pub(super) async fn list_apps(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(params): Query<HashMap<String, String>>,
) -> AppsResult<Json<AdminOauthAppPage>> {
    admin.require(CLASS_WRITE, AREA_APPS)?;
    let search = search_params(&params)?;
    let page = oauth_app::search(&state.db, &search).await?;
    debug!(
        shown = page.apps.len(),
        count = page.count,
        "admin oauth apps listed"
    );
    Ok(Json(AdminOauthAppPage {
        apps: AdminOauthApp::from_rows(&page.apps),
        count: page.count,
        page_size: search.page_size,
    }))
}

/// The listing query parameters: the shared lenient `page` / `page_size`
/// reading, the two exact-text filters (an empty parameter filters
/// nothing, as everywhere in [`super`]), and `trusted`.
///
/// # Errors
///
/// [`AppError::BadRequest`] for an unusable `trusted`.
fn search_params(params: &HashMap<String, String>) -> AppResult<oauth_app::AppSearchParams> {
    Ok(oauth_app::AppSearchParams {
        name: super::super::text_of(params, "name"),
        client_id: super::super::text_of(params, "client_id"),
        trusted: query_trusted(params)?,
        page: super::super::page_number(params, "page", super::super::DEFAULT_PAGE),
        page_size: super::super::page_size(params),
    })
}

/// The `trusted` query parameter as a filter: the same boolean spellings
/// the body flex readers accept (`true`/`false`, `1`/`0`,
/// case-insensitively); anything else is the 400 naming the parameter.
///
/// # Errors
///
/// [`AppError::BadRequest`] for a non-boolean text.
fn query_trusted(params: &HashMap<String, String>) -> AppResult<Option<bool>> {
    let Some(text) = super::super::text_of(params, "trusted") else {
        return Ok(None);
    };
    match text.to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(Some(true)),
        "false" | "0" => Ok(Some(false)),
        _ => Err(AppError::BadRequest(format!(
            "trusted must be a boolean, not {text}"
        ))),
    }
}

/// `POST /api/v1/pleroma/admin/oauth_app` — Pleroma's `create`: validate
/// the body, mint the credentials through [`oauth_app::create_as_admin`]
/// and answer the stored entity.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppsError::Invalid`] for the blank
/// required fields and an unusable scope; [`AppError::Database`] from the
/// insert.
pub(super) async fn create_app(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiBody(body): ApiBody<AppBody>,
) -> AppsResult<Json<AdminOauthApp>> {
    admin.require(CLASS_WRITE, AREA_APPS)?;
    let prepared = prepare(body, true)?;
    let form = AdminNewApp {
        client_name: prepared.name.unwrap_or_default(),
        redirect_uris: prepared.redirect_uris.unwrap_or_default(),
        scopes: prepared.scopes.unwrap_or_default(),
        website: prepared.website,
        trusted: prepared.trusted.unwrap_or(false),
    };
    let app = oauth_app::create_as_admin(&state.db, &form).await?;
    debug!(id = %app.id, trusted = app.trusted, "admin oauth app created");
    Ok(Json(AdminOauthApp::from_app(&app)))
}

/// `PATCH /api/v1/pleroma/admin/oauth_app/{id}` — Pleroma's `update`:
/// the present-only write over the same fields (each one checked exactly
/// as on create when present), answering the updated entity.
///
/// # Errors
///
/// As [`create_app`]'s gate and refusals, plus the 404 of [`load`] and
/// the update read it runs.
pub(super) async fn update_app(
    State(state): State<AppState>,
    admin: AdminUser,
    AxumPath(raw): AxumPath<String>,
    ApiBody(body): ApiBody<AppBody>,
) -> AppsResult<Json<AdminOauthApp>> {
    admin.require(CLASS_WRITE, AREA_APPS)?;
    let existing = load(&state, &raw).await?;
    let prepared = prepare(body, false)?;
    let changes = AppChanges {
        client_name: prepared.name,
        redirect_uris: prepared.redirect_uris,
        scopes: prepared.scopes,
        website: prepared.website,
        trusted: prepared.trusted,
    };
    let app = oauth_app::update(&state.db, existing.id, &changes)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    debug!(id = %app.id, "admin oauth app updated");
    Ok(Json(AdminOauthApp::from_app(&app)))
}

/// `DELETE /api/v1/pleroma/admin/oauth_app/{id}` — Pleroma's `delete`:
/// 204 once the row is gone.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; the 404 of [`load`], or the same 404 when
/// the row vanished between the two reads; [`AppError::Database`] from
/// both.
pub(super) async fn delete_app(
    State(state): State<AppState>,
    admin: AdminUser,
    AxumPath(raw): AxumPath<String>,
) -> AppsResult<StatusCode> {
    admin.require(CLASS_WRITE, AREA_APPS)?;
    let existing = load(&state, &raw).await?;
    if !oauth_app::delete(&state.db, existing.id).await? {
        return Err(AppError::RouteNotFound.into());
    }
    debug!(id = %existing.id, "admin oauth app deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// The app the path names: the row behind a uuid `raw`, 404
/// `{"error":"Not found"}` for text that is no id or no stored row.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] for either miss; [`AppError::Database`]
/// from the read.
async fn load(state: &AppState, raw: &str) -> AppsResult<oauth_app::OauthApp> {
    let Some(id) = parse_id(raw) else {
        return Err(AppError::RouteNotFound.into());
    };
    let app = oauth_app::get_by_id(&state.db, id)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    Ok(app)
}

#[cfg(test)]
mod tests;
