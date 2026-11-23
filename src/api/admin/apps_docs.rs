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
//! action — the listing included, since [`AdminOauthApp`] carries every
//! `client_secret` — with `admin:write`, and so does this port: all four
//! app routes ask `admin:write` or `admin:write:oauth_apps`, and a read
//! grant (`admin:read`, `admin:read:oauth_apps`) opens none of them.
//! The documents are gated `admin:read` / `admin:write` per action, in
//! the area `instance_documents`. Every admin route takes the
//! [`AdminUser`] extractor and its [`require`] scope gate.
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
use axum::{
    Router,
    routing::{get, patch},
};

use self::{
    apps::{create_app, delete_app, list_apps, update_app},
    documents::{delete_document, serve_document, show_document, update_document},
};
#[cfg(doc)]
use crate::{auth::admin::AdminUser, entities::admin::AdminOauthApp};
use crate::{config::Config, extract::multipart::upload_body_limit, state::AppState};

mod apps;
mod documents;

/// The read endpoint class of [`AdminUser::require`] (the documents'
/// reads; every app route is a write).
const CLASS_READ: &str = "read";
/// The write endpoint class of [`AdminUser::require`].
const CLASS_WRITE: &str = "write";
/// The admin scope area of the app routes
/// (`admin:write:oauth_apps`).
const AREA_APPS: &str = "oauth_apps";
/// The admin scope area of the instance-document routes
/// (`admin:read:instance_documents` / `admin:write:instance_documents`).
const AREA_DOCUMENTS: &str = "instance_documents";
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
