//! The admin's OAuth-app maintenance and instance-document endpoints under
//! `/api/v1/pleroma/admin`, plus the public instance-document route
//! (WBE-205).
//!
//! The references are Pleroma's `AdminAPI.OAuthAppController`
//! (`lib/pleroma/web/admin_api/controllers/o_auth_app_controller.ex`) and
//! `AdminAPI.InstanceDocumentController`
//! (`lib/pleroma/web/admin_api/controllers/instance_document_controller.ex`)
//! over `Pleroma.Web.InstanceDocument`
//! (`lib/pleroma/web/instance_document.ex`). Pleroma gates every app
//! action with `admin:write` and the documents with `admin:read` /
//! `admin:write` per action; the ticket splits the apps the way every
//! other admin read is split here — the read asks `admin:read:oauth_apps`,
//! the writes `admin:write:oauth_apps` — and documents live in the area
//! `instance_documents`. Every admin route takes the [`AdminUser`]
//! extractor and its [`require`] scope gate.
//!
//! The OAuth apps:
//!
//! - `GET /oauth_app` (`index`) — the [`search`] listing: `name` and
//!   `client_id` as exact filters, `trusted` as a boolean filter, one
//!   `page` / `page_size` window (the admin listings' defaults: page 1,
//!   50 rows, capped at 200) newest app first, answered as
//!   `{apps, count, page_size}`. Like Pleroma's `App.search` the `count`
//!   aggregates the whole table, not the filtered query, so the total
//!   does not move with the filters. Pleroma's spec quietly defaults
//!   `trusted` to `false` and then filters on it — its plain index hides
//!   every trusted app; the parameter is optional here, and absent means
//!   both kinds.
//! - `POST /oauth_app` (`create`) — `name` and `redirect_uris` (one
//!   string or a list, normalized exactly as client registration
//!   normalizes it) are required; `scopes` (a string or a list, default
//!   `read`, every name validated per decision 9), `website` and
//!   `trusted` are not. A blank or absent required field is the 400
//!   `{"errors":{"name":["can't be blank"]}}` Pleroma's `CastAndValidate`
//!   renders for its changeset refusals (`App.errors` maps the
//!   `client_name` error onto `name`), an unusable scope likewise; the
//!   stored row answers as the seven-key [`AdminOauthApp`] entity.
//! - `PATCH /oauth_app/{id}` (`update`) — the present-only write over the
//!   same fields (each one checked exactly as on create when present, so
//!   a sent-but-blank `name` still refuses), answering the updated
//!   entity. A `website` of JSON `null` reads as absent: the stored
//!   homepage stays. An id that is no uuid or no row is 404
//!   `{"error":"Not found"}` — the ticket's pinned refusal, matching the
//!   `FallbackController` 404 Pleroma's integer-id miss reaches.
//! - `DELETE /oauth_app/{id}` (`delete`) — 204 once the row is gone, the
//!   same 404 for an unknown id.
//!
//! `trusted` is stored and rendered but not yet acted on: a trusted app
//! may skip the `/oauth/authorize` consent form, the way Pleroma's
//! `OAuthController` skips it for `app.trusted`. The flag is the admin's
//! to set through these routes; acting on it in the authorize flow is
//! left to the ticket that reworks that page — see the note there
//! ([`crate::api::oauth::authorize`]).
//!
//! The instance documents are static HTML pages (terms of service and
//! friends), stored as `<name>.html` under `INSTANCE_DOCS_DIR` and named
//! by `INSTANCE_DOCUMENT_NAMES` — the ticket's config list, the data
//! spelling of Pleroma's `InstanceDocument` map:
//!
//! - `GET /instance_document/{name}` (`show`) — the file bytes with
//!   `content-type: text/html`, or 404 for a name off the list or a
//!   document never uploaded.
//! - `PATCH /instance_document/{name}` (`update`) — the multipart `file`
//!   part, at most 1 MiB, written atomically (a hidden staging file
//!   renamed into place, the same move the frontend installer makes);
//!   200 `{"url":"{BASE_URL}/instance/<name>.html"}`. A body without the
//!   part is 400 `{"error":"Missing parameter: file"}`, an oversize one
//!   413.
//! - `DELETE /instance_document/{name}` (`delete`) — 200 `{}` once the
//!   file is gone, 404 when it was not there.
//! - `GET /instance/{name}.html` ([`public_router`], merged at the root
//!   of [`crate::app`]) — the public read: no auth,
//!   `text/html; charset=utf-8`, and the ticket's
//!   `Content-Security-Policy` that lets the page style itself and show
//!   images but nothing else. An unlisted or absent document is the JSON
//!   404 `{"error":"Not found"}`.
//!
//! Only names from `INSTANCE_DOCUMENT_NAMES` are ever touched, and a
//! configured name must itself be a safe single path segment
//! (`is_document_name`) — so a `../`-shaped path parameter can never
//! name a file outside the store, whatever an operator configures.
//!
//! [`AdminUser`]: crate::auth::admin::AdminUser
//! [`require`]: crate::auth::admin::AdminUser::require
//! [`search`]: crate::models::oauth_app::search

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use axum::{
    Json, Router,
    extract::{Path as AxumPath, Query, State},
    http::{
        StatusCode,
        header::{CONTENT_SECURITY_POLICY, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
    routing::{get, patch},
};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tracing::debug;

use crate::{
    api::apps::{RedirectUris, normalize_redirect_uris},
    auth::admin::AdminUser,
    config::Config,
    entities::admin::{AdminOauthApp, AdminOauthAppPage, InstanceDocumentAck, InstanceDocumentUrl},
    error::{AppError, AppResult},
    extract::{
        ApiBody, flex,
        multipart::{MultipartForm, UploadedFiles as _, upload_body_limit},
    },
    models::oauth_app::{self, AdminNewApp, AppChanges},
    oauth::scopes::Scopes,
    state::AppState,
    util::id::{new_id, parse_id},
};

/// The read endpoint class of [`AdminUser::require`].
const CLASS_READ: &str = "read";
/// The write endpoint class of [`AdminUser::require`].
const CLASS_WRITE: &str = "write";
/// The admin scope area of the app routes
/// (`admin:read:oauth_apps` / `admin:write:oauth_apps`).
const AREA_APPS: &str = "oauth_apps";
/// The admin scope area of the instance-document routes
/// (`admin:read:instance_documents` / `admin:write:instance_documents`).
const AREA_DOCUMENTS: &str = "instance_documents";
/// The refusal Ecto renders for a blank required field, the message the
/// ticket pins.
const CANNOT_BE_BLANK: &str = "can't be blank";
/// 400 body of a document upload whose body carries no `file` part, in
/// the `Missing parameter: <key>` spelling the admin API shares.
const MISSING_FILE: &str = "Missing parameter: file";
/// The size ceiling of one instance document, the ticket's 1 MiB.
const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
/// 413 body of an oversize document upload, naming the ceiling.
const DOCUMENT_TOO_LARGE: &str = "File is too large (maximum is 1048576 bytes)";
/// `content-type` of the admin document read, as Pleroma's `show` sets
/// it.
const HTML_TYPE: &str = "text/html";
/// `content-type` of the public document read, with its charset.
const HTML_UTF8_TYPE: &str = "text/html; charset=utf-8";
/// The public document route's `Content-Security-Policy`: the page may
/// style itself and show images, and do nothing else.
const DOCUMENT_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src * data:";
/// The multipart field the document upload reads its file from.
const FILE_FIELD: &str = "file";
/// The extension every instance document is stored and served under.
const DOCUMENT_EXTENSION: &str = "html";

/// The `/oauth_app` and `/instance_document` routes of the admin API,
/// merged into [`super`].
///
/// `config` sizes the upload body limit through [`upload_body_limit`], so
/// an oversize document meets the handler's own 1 MiB refusal rather than
/// the multipart extractor's 2 MiB default one.
pub fn router(config: &Config) -> Router<AppState> {
    Router::new()
        .route("/oauth_app", get(list_apps).post(create_app))
        .route("/oauth_app/{id}", patch(update_app).delete(delete_app))
        .route(
            "/instance_document/{name}",
            get(show_document)
                .patch(update_document)
                .delete(delete_document),
        )
        .layer(upload_body_limit(config))
}

/// The public `GET /instance/{name}.html` router, merged at the top level
/// of [`crate::app`]: the address the upload answer names as the
/// document's own.
///
/// The router forbids a parameter with a static suffix in one segment, so
/// the route takes the whole last segment and `serve_document` holds the
/// `.html` spelling: every other segment under `/instance/` is the 404.
pub fn public_router() -> Router<AppState> {
    Router::new().route("/instance/{name}", get(serve_document))
}

/// What an app handler can fail with: the project error as-is, or
/// Pleroma's `CastAndValidate` 400 naming the refused fields.
#[derive(Debug)]
enum AppsError {
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
type AppsResult<T> = Result<T, AppsError>;

/// The body of `POST /oauth_app` and `PATCH /oauth_app/{id}`: every
/// mutable field of the entity, read as an `Option` so `create` can name
/// its own missing-field refusals and `update` can tell absent from
/// present. `redirect_uris` takes the two spellings client registration
/// takes; `trusted` deserializes through [`flex::opt_bool`] so a
/// form-encoded `"true"` counts.
#[derive(Debug, Deserialize)]
struct AppBody {
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
/// and its `admin:read` gate; [`AppError::BadRequest`] for a `trusted`
/// that is no boolean; [`AppError::Database`] from the two reads.
async fn list_apps(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(params): Query<HashMap<String, String>>,
) -> AppsResult<Json<AdminOauthAppPage>> {
    admin.require(CLASS_READ, AREA_APPS)?;
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
        name: super::text_of(params, "name"),
        client_id: super::text_of(params, "client_id"),
        trusted: query_trusted(params)?,
        page: super::page_number(params, "page", super::DEFAULT_PAGE),
        page_size: super::page_size(params),
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
    let Some(text) = super::text_of(params, "trusted") else {
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
async fn create_app(
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
async fn update_app(
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
async fn delete_app(
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

/// `GET /api/v1/pleroma/admin/instance_document/{name}` — Pleroma's
/// `show`: the stored bytes as `text/html`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::RouteNotFound`] for a name off
/// the configured list or a document never uploaded;
/// [`AppError::Internal`] for a store that refuses to be read.
async fn show_document(
    State(state): State<AppState>,
    admin: AdminUser,
    AxumPath(name): AxumPath<String>,
) -> AppResult<Response> {
    admin.require(CLASS_READ, AREA_DOCUMENTS)?;
    let bytes = read_document(&state.config, &name).await?;
    Ok(([(CONTENT_TYPE, HTML_TYPE)], bytes).into_response())
}

/// `PATCH /api/v1/pleroma/admin/instance_document/{name}` — Pleroma's
/// `update`: the multipart `file` copied into the store and its public
/// URL answered.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::RouteNotFound`] for a name off
/// the configured list; [`AppError::BadRequest`] for a body without the
/// `file` part; [`AppError::PayloadTooLarge`] for a file over
/// [`MAX_DOCUMENT_BYTES`]; [`AppError::Internal`] for a store that
/// refuses to be written.
async fn update_document(
    State(state): State<AppState>,
    admin: AdminUser,
    AxumPath(name): AxumPath<String>,
    mut form: MultipartForm<serde::de::IgnoredAny>,
) -> AppResult<Json<InstanceDocumentUrl>> {
    admin.require(CLASS_WRITE, AREA_DOCUMENTS)?;
    let path = document_path(&state.config, &name).ok_or(AppError::RouteNotFound)?;
    let file = form
        .files
        .take(FILE_FIELD)
        .ok_or_else(|| AppError::BadRequest(MISSING_FILE.to_owned()))?;
    let size = file.bytes.len();
    if size > MAX_DOCUMENT_BYTES {
        return Err(AppError::PayloadTooLarge(DOCUMENT_TOO_LARGE.to_owned()));
    }
    store_document(path, file.bytes).await?;
    debug!(document = %name, size, "instance document stored");
    Ok(Json(InstanceDocumentUrl {
        url: document_url(&state.config, &name),
    }))
}

/// `DELETE /api/v1/pleroma/admin/instance_document/{name}` — Pleroma's
/// `delete`: the file gone, the empty object back.
///
/// # Errors
///
/// As [`show_document`]'s gate and refusals; a store that refuses the
/// removal for any reason but absence is [`AppError::Internal`].
async fn delete_document(
    State(state): State<AppState>,
    admin: AdminUser,
    AxumPath(name): AxumPath<String>,
) -> AppResult<Json<InstanceDocumentAck>> {
    admin.require(CLASS_WRITE, AREA_DOCUMENTS)?;
    let path = document_path(&state.config, &name).ok_or(AppError::RouteNotFound)?;
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {
            debug!(document = %name, "instance document deleted");
            Ok(Json(InstanceDocumentAck))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(AppError::RouteNotFound),
        Err(err) => Err(internal_store_failure(&err)),
    }
}

/// `GET /instance/{name}.html` — the public read of a document: bytes as
/// `text/html; charset=utf-8` behind the document CSP, no auth. The
/// matched segment carries the `.html` suffix the route pattern cannot
/// pin; stripped here, refused without it.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] for a segment that is not a configured
/// document's `<name>.html` or a document never uploaded;
/// [`AppError::Internal`] for a store that refuses to be read.
async fn serve_document(
    State(state): State<AppState>,
    AxumPath(segment): AxumPath<String>,
) -> AppResult<Response> {
    let suffix = format!(".{DOCUMENT_EXTENSION}");
    let name = segment
        .strip_suffix(suffix.as_str())
        .ok_or(AppError::RouteNotFound)?;
    let bytes = read_document(&state.config, name).await?;
    Ok((
        [
            (CONTENT_TYPE, HTML_UTF8_TYPE),
            (CONTENT_SECURITY_POLICY, DOCUMENT_CSP),
        ],
        bytes,
    )
        .into_response())
}

/// The stored path of a configured document name: `<dir>/<name>.html`,
/// `None` for every name the instance does not offer — off the list, or
/// not a safe segment. That is also the traversal gate: a `../` from a
/// path parameter names no file, and no write can escape the directory.
fn document_path(config: &Config, name: &str) -> Option<PathBuf> {
    if !is_document_name(name) {
        return None;
    }
    config
        .statics
        .documents
        .names
        .iter()
        .find(|configured| configured.as_str() == name)
        .map(|_| {
            config
                .statics
                .documents
                .dir
                .join(format!("{name}.{DOCUMENT_EXTENSION}"))
        })
}

/// Whether `name` may stand as one document segment: the same filename
/// charset the frontend installer keeps, non-empty, and neither `.` nor
/// `..`.
fn is_document_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The public URL of `name`: the address [`public_router`] serves it at.
fn document_url(config: &Config, name: &str) -> String {
    format!("{}/instance/{name}.{DOCUMENT_EXTENSION}", config.base_url)
}

/// The document bytes behind a configured name.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] for the unlisted and the never-uploaded;
/// [`AppError::Internal`] for any other read failure.
async fn read_document(config: &Config, name: &str) -> AppResult<Bytes> {
    let path = document_path(config, name).ok_or(AppError::RouteNotFound)?;
    match tokio::fs::read(&path).await {
        Ok(bytes) => Ok(Bytes::from(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(AppError::RouteNotFound),
        Err(err) => Err(internal_store_failure(&err)),
    }
}

/// Writes the document atomically: the bytes land in a hidden staging
/// file beside the target (same directory, so the rename is a plain
/// move) and the rename swaps them in, replacing whatever stood there.
/// The filesystem work runs on the blocking pool, the way the frontend
/// installer's does.
///
/// # Errors
///
/// [`AppError::Internal`] for a store that cannot be created, written or
/// renamed, and for a blocking task that did not finish.
async fn store_document(path: PathBuf, bytes: Bytes) -> AppResult<()> {
    tokio::task::spawn_blocking(move || write_document_blocking(&path, &bytes))
        .await
        .map_err(|err| {
            AppError::Internal(format!(
                "instance document write task did not finish: {err}"
            ))
        })?
}

/// The blocking half of [`store_document`]: directory, staging file,
/// rename.
fn write_document_blocking(path: &Path, bytes: &[u8]) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| internal_store_failure(&err))?;
    }
    let staging = staging_path(path);
    write_then_rename(&staging, path, bytes)
}

/// The staging sibling of `path`: a hidden dotfile in the same directory,
/// unique per attempt, so two concurrent uploads never share a file and
/// a reader never sees a half-written document.
fn staging_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    path.with_file_name(format!(".{name}.tmp-{}", new_id()))
}

/// Writes `bytes` to `staging` and renames it over `path`, removing the
/// staging file when only the rename failed.
fn write_then_rename(staging: &Path, path: &Path, bytes: &[u8]) -> AppResult<()> {
    std::fs::write(staging, bytes).map_err(|err| internal_store_failure(&err))?;
    match std::fs::rename(staging, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _cleanup = std::fs::remove_file(staging);
            Err(internal_store_failure(&err))
        }
    }
}

/// A filesystem failure that is not the client's: a 500 whose detail is
/// logged, never sent.
fn internal_store_failure(err: &std::io::Error) -> AppError {
    AppError::Internal(format!("the instance document store said: {err}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// An [`AppBody`] with every field absent, the empty-patch shape.
    fn empty_body() -> AppBody {
        AppBody {
            name: None,
            redirect_uris: None,
            scopes: None,
            website: None,
            trusted: None,
        }
    }

    /// An [`AppBody`] named `n` with the oob redirect, carrying `scopes`.
    fn body_with(scopes: Option<ScopeInput>) -> AppBody {
        AppBody {
            name: Some("n".to_owned()),
            redirect_uris: Some(RedirectUris::Single("urn:ietf:wg:oauth:2.0:oob".to_owned())),
            scopes,
            website: None,
            trusted: None,
        }
    }

    /// Whether `err` is the invalid-fields 400 naming `field` and `message`.
    fn is_invalid(err: &AppsError, field: &str, message: &str) -> bool {
        matches!(
            err,
            AppsError::Invalid(refusals)
                if refusals.iter().any(|(got, got_message)| *got == field && got_message == message)
        )
    }

    /// The body parses: the ticket's JSON shapes, the form request's
    /// string booleans and bracket lists, and an empty body as
    /// all-`None`.
    #[test]
    fn the_body_parses_the_ticket_shape() {
        let full: AppBody = serde_json::from_value(json!({
            "name": "My App",
            "redirect_uris": ["https://myapp.com/cb"],
            "scopes": ["read", "write"],
            "website": "https://myapp.com/",
            "trusted": true,
        }))
        .expect("the ticket's shape deserializes");
        assert_eq!(full.name.as_deref(), Some("My App"));
        assert!(
            matches!(full.redirect_uris, Some(RedirectUris::Multiple(_))),
            "a list is the list arm"
        );
        assert!(matches!(full.scopes, Some(ScopeInput::List(_))));
        assert_eq!(full.trusted, Some(true), "a real boolean");

        let form: AppBody = serde_json::from_value(json!({
            "trusted": "false",
            "scopes": "read+write",
            "redirect_uris": "urn:ietf:wg:oauth:2.0:oob",
        }))
        .expect("the form spelling reads through flex");
        assert_eq!(form.trusted, Some(false), "the stringly-typed flag");
        assert!(matches!(form.scopes, Some(ScopeInput::Single(_))));
        assert!(matches!(form.redirect_uris, Some(RedirectUris::Single(_))));

        let empty: AppBody = serde_json::from_value(json!({})).expect("empty body");
        assert!(
            empty.name.is_none()
                && empty.redirect_uris.is_none()
                && empty.scopes.is_none()
                && empty.website.is_none()
                && empty.trusted.is_none(),
            "every field absent"
        );
    }

    /// The create-side gates: absent and blank refuse with the pinned
    /// message, a sent-but-normalized-empty URI list refuses too, and
    /// every refusal lands in one answer.
    #[test]
    fn create_refuses_the_blank_required_fields() {
        let err = prepare(empty_body(), true).expect_err("nothing creates");
        assert!(is_invalid(&err, "name", CANNOT_BE_BLANK), "{err:?}");
        assert!(
            is_invalid(&err, "redirect_uris", CANNOT_BE_BLANK),
            "both refusals in one answer"
        );

        let blank = AppBody {
            name: Some("   ".to_owned()),
            redirect_uris: Some(RedirectUris::Multiple(vec![" ".to_owned(), String::new()])),
            ..empty_body()
        };
        let blank_err = prepare(blank, true).expect_err("blanks create nothing");
        assert!(
            is_invalid(&blank_err, "name", CANNOT_BE_BLANK),
            "{blank_err:?}"
        );
        assert!(
            is_invalid(&blank_err, "redirect_uris", CANNOT_BE_BLANK),
            "the blank list normalizes to blank"
        );

        let good = AppBody {
            name: Some("  Fine  ".to_owned()),
            redirect_uris: Some(RedirectUris::Single("urn:ietf:wg:oauth:2.0:oob".to_owned())),
            ..empty_body()
        };
        let prepared = prepare(good, true).expect("a sent pair creates");
        assert_eq!(prepared.name.as_deref(), Some("Fine"), "the name trims");
        assert_eq!(
            prepared.redirect_uris.as_deref(),
            Some("urn:ietf:wg:oauth:2.0:oob"),
            "a single uri passes through verbatim"
        );
        assert_eq!(
            prepared.scopes.as_deref(),
            Some("read"),
            "the create default"
        );
        assert_eq!(prepared.trusted, None, "the flag defaults at the handler");
    }

    /// The update-side gates: absent changes nothing, present-but-blank
    /// still refuses.
    #[test]
    fn update_checks_only_the_fields_it_received() {
        let prepared = prepare(empty_body(), false).expect("an empty patch changes neither field");
        assert!(
            prepared.name.is_none()
                && prepared.redirect_uris.is_none()
                && prepared.scopes.is_none(),
            "nothing to write"
        );

        let blank = AppBody {
            name: Some(String::new()),
            ..empty_body()
        };
        let err = prepare(blank, false).expect_err("a present blank still refuses");
        assert!(is_invalid(&err, "name", CANNOT_BE_BLANK), "{err:?}");
    }

    /// Scopes: the create default, both spellings, admin-scope names,
    /// and the refusal wording for junk.
    #[test]
    fn scopes_default_validate_and_store() {
        let parsed = prepare(
            body_with(Some(ScopeInput::Single("read+write".to_owned()))),
            true,
        )
        .expect("a string parses");
        assert_eq!(parsed.scopes.as_deref(), Some("read write"));

        let listed = Some(ScopeInput::List(vec![
            "admin:read".to_owned(),
            "read".to_owned(),
        ]));
        let order = prepare(body_with(listed), true).expect("a list parses too");
        assert_eq!(
            order.scopes.as_deref(),
            Some("admin:read read"),
            "parse order survives, and admin scopes register here"
        );

        let err = prepare(
            body_with(Some(ScopeInput::Single("bogus".to_owned()))),
            true,
        )
        .expect_err("an unknown scope refuses");
        assert!(
            is_invalid(
                &err,
                "scopes",
                "invalid scopes: unknown top-level scope: bogus"
            ),
            "{err:?}"
        );
    }

    /// The trusted query parameter: absent filters nothing, every boolean
    /// spelling maps, and junk is the 400 naming it.
    #[test]
    fn the_trusted_filter_reads_booleans_only() {
        let params: HashMap<String, String> = HashMap::new();
        assert_eq!(query_trusted(&params).expect("absent is no filter"), None);
        for (text, want) in [
            ("true", Some(true)),
            ("TRUE", Some(true)),
            ("1", Some(true)),
            ("false", Some(false)),
            ("0", Some(false)),
        ] {
            let mut one = HashMap::new();
            let replaced = one.insert("trusted".to_owned(), text.to_owned());
            drop(replaced);
            assert_eq!(
                query_trusted(&one).expect(text),
                want,
                "`{text}` reads as {want:?}"
            );
        }
        let mut junk = HashMap::new();
        let replaced = junk.insert("trusted".to_owned(), "yes".to_owned());
        drop(replaced);
        let err = query_trusted(&junk).expect_err("junk is refused");
        assert_eq!(err.to_string(), "trusted must be a boolean, not yes");
    }

    /// The listing window: the admin defaults, the shared cap, and the
    /// filters passing through.
    #[test]
    fn the_listing_reads_the_admin_window() {
        let parsed = search_params(&HashMap::new()).expect("an empty query lists");
        assert_eq!(parsed.page, super::super::DEFAULT_PAGE, "page defaults");
        assert_eq!(parsed.page_size, 50, "and the admin 50-row window");
        assert!(
            parsed.name.is_none() && parsed.client_id.is_none() && parsed.trusted.is_none(),
            "no filters"
        );
        let mut pairs = HashMap::new();
        for (key, value) in [("page", "3"), ("page_size", "5000"), ("name", "x")] {
            let replaced = pairs.insert(key.to_owned(), value.to_owned());
            drop(replaced);
        }
        let capped = search_params(&pairs).expect("the window parses");
        assert_eq!(capped.page, 3, "a sent page");
        assert_eq!(capped.page_size, 200, "the shared cap");
        assert_eq!(capped.name.as_deref(), Some("x"), "the filter passes");
    }

    /// The document path: only configured, segment-safe names resolve,
    /// and every traversal-shaped or unlisted name is `None`.
    #[test]
    fn only_listed_safe_names_find_a_document_path() {
        let mut config = Config::for_tests();
        config.statics.documents.names = vec!["terms-of-service".to_owned()];
        assert_eq!(
            document_path(&config, "terms-of-service"),
            Some(config.statics.documents.dir.join("terms-of-service.html")),
            "the configured name maps to <dir>/<name>.html"
        );
        for refused in [
            "instance-panel",
            "",
            ".",
            "..",
            "../evil",
            "..%2fevil",
            "a/b",
            "terms of service",
        ] {
            assert!(
                document_path(&config, refused).is_none(),
                "`{refused}` is no document path"
            );
        }
    }

    /// The name gate itself: the plain-segment charset, nothing else.
    #[test]
    fn document_names_are_plain_segments() {
        for ok in ["terms-of-service", "about", "a_b.c-1", ".hidden"] {
            assert!(is_document_name(ok), "`{ok}` is a name");
        }
        for bad in ["", ".", "..", "../x", "a/b", "a\\b", "a b", "über", "~home"] {
            assert!(!is_document_name(bad), "`{bad}` is not a name");
        }
    }

    /// The answer URL hangs off the instance origin, at the public route.
    #[test]
    fn the_document_url_is_the_public_address() {
        let config = Config::for_tests();
        assert_eq!(
            document_url(&config, "terms-of-service"),
            "https://test.example/instance/terms-of-service.html"
        );
    }

    /// The staging sibling sits next to the target, hidden and uniquely
    /// named, so a crash or a concurrent upload never exposes a partial
    /// document.
    #[test]
    fn the_staging_sibling_is_a_hidden_unique_name_in_the_directory() {
        let path = Path::new("/srv/instance/terms-of-service.html");
        let staging = staging_path(path);
        let file_name = staging
            .file_name()
            .and_then(|name| name.to_str())
            .expect("a name");
        assert!(
            file_name.starts_with(".terms-of-service.html.tmp-"),
            "hidden and suffixed: {file_name}"
        );
        assert_eq!(staging.parent(), path.parent(), "same directory");
        assert_ne!(staging, staging_path(path), "two stagings never collide");
    }

    /// A round trip against a real temporary directory: the first store
    /// creates the directory and the file, a re-store replaces it whole,
    /// and nothing is left staged.
    #[tokio::test]
    async fn documents_round_trip_through_the_store() {
        let dir = tempfile::tempdir().expect("a docs dir");
        let path = dir.path().join("deep").join("terms-of-service.html");

        store_document(path.clone(), Bytes::from_static(b"<h1>terms</h1>"))
            .await
            .expect("the first store works, creating the parent");
        assert_eq!(
            std::fs::read(&path).expect("readable"),
            b"<h1>terms</h1>",
            "the bytes landed whole"
        );

        store_document(path.clone(), Bytes::from_static(b"<h1>new</h1>"))
            .await
            .expect("the second store replaces");
        assert_eq!(
            std::fs::read(&path).expect("readable"),
            b"<h1>new</h1>",
            "and the new bytes are the whole file"
        );

        let deep = dir.path().join("deep");
        let strays: Vec<String> = std::fs::read_dir(&deep)
            .expect("the dir reads")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            strays,
            vec!["terms-of-service.html".to_owned()],
            "no staging left"
        );
    }

    /// A store path the process cannot write is an internal failure, not
    /// a client error dressed up.
    #[tokio::test]
    async fn an_unwritable_store_is_an_internal_failure() {
        let dir = tempfile::tempdir().expect("a docs dir");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").expect("a file to block with");
        let behind_file = blocker.join("terms-of-service.html");
        let err = store_document(behind_file, Bytes::from_static(b"x"))
            .await
            .expect_err("the store cannot write through a file");
        assert!(
            matches!(err, AppError::Internal(_)),
            "a filesystem refusal is a 500: {err:?}"
        );
    }

    /// The two error halves render their pinned bodies: the invalid arm
    /// the 400 `{"errors":{…}}` map, the wrapped arm the usual
    /// [`AppError`].
    #[test]
    fn the_error_halves_render_the_pinned_bodies() {
        let response =
            AppsError::Invalid(vec![("name", CANNOT_BE_BLANK.to_owned())]).into_response();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "the validation refusal is a 400"
        );
        let wrapped = AppsError::App(AppError::RouteNotFound).into_response();
        assert_eq!(
            wrapped.status(),
            StatusCode::NOT_FOUND,
            "the wrapped arm keeps its status"
        );
    }
}
