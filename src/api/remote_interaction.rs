//! Remote interaction: the endpoints a remote instance uses to follow a
//! local account or interact with a local object from its own frontend
//! (`docs/ARCHITECTURE.md` decision 1).
//!
//! The reference is Pleroma's `RemoteInteractionController`
//! (`lib/pleroma/web/remote_interaction/remote_interaction_controller.ex`)
//! and the routes `lib/pleroma/web/router.ex` serves it on; the ticket
//! rules here are authoritative where the two differ. The four surfaces:
//!
//! - `POST /api/v1/pleroma/remote_interaction` (no auth, form or JSON):
//!   given `ap_id` (a local actor or object URL on *this* instance) and
//!   `profile` (the visitor's remote `user@domain` handle), `WebFinger`s
//!   the profile, reads the link with
//!   `rel="http://ostatus.org/schema/1.0/subscribe"` and answers its
//!   template with `{uri}` replaced by the percent-encoded `ap_id`. A
//!   profile that resolves to no subscribe template is the 400
//!   `{"error":"Couldn't find user"}`; an absent or blank parameter is
//!   the 400 `{"error":"Missing parameter: ap_id"}` (or `profile`).
//!   `ap_id` itself is not validated: it is the value the remote instance
//!   is asked to carry back, and Pleroma substitutes whatever it is
//!   given.
//! - `GET /ostatus_subscribe?acct=<handle>` renders the login form that
//!   posts `acct`, `authorization[name]` and `authorization[password]`
//!   to `POST /ostatus_subscribe`, which authenticates the local user,
//!   resolves the handle through [`get_or_fetch_by_acct`] and records
//!   the follow — the success page on its way out, the form with an
//!   error line on any failure.
//! - `GET /authorize_interaction?uri=<url or handle>` sends the browser
//!   to the frontend page for whatever the `uri` resolves to: an actor to
//!   `{FRONTEND_BASE_URL}/users/{id}`, a status to
//!   `{FRONTEND_BASE_URL}/notice/{id}`, nothing resolvable to the 404
//!   JSON.
//! - `GET /main/ostatus` renders the remote-subscribe form (the local
//!   account under `user[nickname]`, the remote handle under
//!   `user[profile]`), and `POST /main/ostatus` computes exactly the URL
//!   the JSON endpoint answers and redirects to it (302), re-rendering
//!   the form with an error line whenever that fails.
//!
//! Every page this module renders is a full HTML document with all
//! request- and database-sourced values escaped, under a
//! `content-security-policy` that allows the one interaction the pages
//! exist for: a form submit to this same origin (see the `PAGE_CSP`
//! constant).
//!
//! The subscribe lookup reads the full `WebFinger` answer through the
//! client's whole-document reader (see
//! [`crate::federation::webfinger_client`]), the same URL, host-meta
//! fallback and format sniffing the actor resolver uses. The mock-server
//! tests at the bottom of this module drive it against loopback, through
//! that client's scheme-and-authority override.

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse as _, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::debug;
use url::form_urlencoded;

use crate::{
    api::{accounts::after_follow_changed, pleroma_account_settings::looks_like_ap_id},
    config::Config,
    error::{AppError, AppResult},
    extract::ApiBody,
    federation::{
        fetch_actor::{get_or_fetch_by_acct, get_or_fetch_by_ap_id},
        fetch_object::resolve_status,
        webfinger_client::{URI_MACRO, URI_MACRO_ENCODED, finger_with_base, strip_finger_prefixes},
    },
    http_client::HttpClient,
    models::{
        follow,
        user::{User, get_by_acct, register::authenticate},
    },
    state::AppState,
    util::sanitize::escape_html,
};

/// The `rel` naming an instance's remote-follow (subscribe) endpoint.
const SUBSCRIBE_REL: &str = "http://ostatus.org/schema/1.0/subscribe";
/// The 400 body message of the JSON endpoint, Pleroma's wording.
const COULD_NOT_FIND_USER: &str = "Couldn't find user";
/// The 400 body of an absent or blank `ap_id` (the shape of the shared
/// `Missing parameter: <name>` refusals, see [`crate::api::accounts`]).
const MISSING_AP_ID: &str = "Missing parameter: ap_id";
/// The 400 body of an absent or blank `profile`.
const MISSING_PROFILE: &str = "Missing parameter: profile";
/// The page error line for a handle this instance cannot resolve,
/// Pleroma's `follow`-page wording.
const COULD_NOT_FIND_USER_PAGE: &str = "Could not find user";
/// The page error line for a failed login or absent credentials,
/// Pleroma's `follow_login` wording.
const WRONG_CREDENTIALS: &str = "Wrong username or password";
/// The page error line for any other failure, Pleroma's catch-all.
const SOMETHING_WENT_WRONG: &str = "Something went wrong.";
/// The title of the `ostatus_subscribe` login page and its success page.
const FOLLOW_TITLE: &str = "Remote follow";
/// The title of the `main/ostatus` remote-subscribe page.
const REMOTE_SUBSCRIBE_TITLE: &str = "Remote subscribe";
/// The success line of the follow page, with the followed handle
/// (escaped) interpolated after it by [`success_body`].
const FOLLOWED_MESSAGE: &str = "You are now following";
/// The path of the login-form pair, the `POST` target of the form
/// [`subscribe_body`] renders.
const SUBSCRIBE_PATH: &str = "/ostatus_subscribe";
/// The path of `GET /authorize_interaction`.
const AUTHORIZE_PATH: &str = "/authorize_interaction";
/// The path of the remote-subscribe form pair.
const REMOTE_SUBSCRIBE_PATH: &str = "/main/ostatus";
/// The media type of every page this module renders.
const PAGE_CONTENT_TYPE: &str = "text/html; charset=utf-8";
/// The `Content-Security-Policy` of every page: no scripts, inline CSS
/// only and a form submit to this same origin. Unlike the API's helper
/// pages (whose `default-src 'none'` with no `form-action` lives in
/// [`crate::api::html_pages`]), these pages exist to submit a form, and
/// a policy without the `form-action 'self'` arm would have the browser
/// refuse it.
const PAGE_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'";
/// The one inline stylesheet [`page_document`] embeds (the CSP allows no
/// other).
const PAGE_STYLE: &str = "body{margin:0;min-height:100vh;display:flex;\
    align-items:center;justify-content:center;background:#f5f4ef;\
    font-family:system-ui,sans-serif;color:#28242d}\
    main{background:#fff;border-radius:8px;padding:2rem;max-width:30rem;\
    box-shadow:0 2px 10px rgba(40,36,45,0.12)}\
    h1{font-size:1.3rem;margin-top:0}\
    label{display:block}\
    input{margin:0.25rem 0 0.75rem 0;padding:0.4rem 0.6rem;width:100%}\
    input[type=submit]{padding:0.5rem 1rem;border-radius:4px;\
    border:1px solid #7c8a52;background:#7c8a52;color:#fff;cursor:pointer;width:auto}\
    p.error{color:#a1282c}";
/// The `Content-Security-Policy` header [`PAGE_CSP`] rides on.
static CSP_HEADER: HeaderName = HeaderName::from_static("content-security-policy");
/// The `X-Frame-Options` header every page puts up: these pages carry
/// login forms, and no other site may frame them.
static FRAME_OPTIONS_HEADER: HeaderName = HeaderName::from_static("x-frame-options");

/// The top-level remote-interaction pages router, merged at the root by
/// [`crate::app`].
///
/// Mounts `/ostatus_subscribe`, `/authorize_interaction` and
/// `/main/ostatus`. Do not call [`Router::fallback`] on it: unmatched
/// paths must reach the app-wide fallback in [`crate::app`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route(SUBSCRIBE_PATH, get(subscribe_form).post(do_subscribe))
        .route(AUTHORIZE_PATH, get(authorize_interaction))
        .route(
            REMOTE_SUBSCRIBE_PATH,
            get(show_remote_subscribe).post(do_remote_subscribe),
        )
}

/// The `/api/v1/pleroma` fragment of this feature, merged by
/// [`crate::api::v1`]: `POST /remote_interaction`.
pub fn pleroma_router() -> Router<AppState> {
    Router::new().route("/pleroma/remote_interaction", post(remote_interaction))
}

/// The parameters of `POST /api/v1/pleroma/remote_interaction`: both are
/// optional at the type level so a request missing one deserializes and
/// is refused with the endpoint's own `Missing parameter` 400 rather than
/// the extractor's generic 422.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct InteractionParams {
    /// The local actor or object URL the interaction is about.
    ap_id: Option<String>,
    /// The visitor's remote `user@domain` handle.
    profile: Option<String>,
}

/// The 200 body of `POST /api/v1/pleroma/remote_interaction`: the
/// interaction URL the client should send the visitor's browser to.
#[derive(Debug, Serialize)]
struct InteractionReply {
    /// The remote instance's subscribe template with the `ap_id`
    /// substituted for its `{uri}` macro.
    url: String,
}

/// The query of `GET /ostatus_subscribe`: Pleroma's `%{"acct" => handle}`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SubscribeQuery {
    /// The remote handle the form will follow.
    acct: Option<String>,
}

/// The body of `POST /ostatus_subscribe`: the hidden `acct` plus the
/// Rails-style nested credentials (`authorization[name]`,
/// `authorization[password]`) the form renders. All fields optional —
/// any missing piece is the re-rendered form, not a parse error.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SubscribeSubmit {
    /// The remote handle to follow.
    acct: Option<String>,
    /// The submitted login.
    authorization: Option<Authorization>,
}

/// The nested `authorization` map of [`SubscribeSubmit`].
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Authorization {
    /// The local account's nickname or email.
    name: Option<String>,
    /// Its password, taken verbatim (never trimmed).
    password: Option<String>,
}

/// The query of `GET /authorize_interaction`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct InteractionQuery {
    /// The actor URL, status URL or `user@domain` handle to interact with.
    uri: Option<String>,
}

/// The query of `GET /main/ostatus`: Pleroma reads the flat
/// `nickname`/`profile` pair when it renders the form.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RemoteSubscribeQuery {
    /// The local account to prefill `user[nickname]` with.
    nickname: Option<String>,
    /// The remote handle to prefill `user[profile]` with.
    profile: Option<String>,
}

/// The body of `POST /main/ostatus`: the fields ride under `user`, the
/// shape of the form [`remote_subscribe_body`] renders (`user[nickname]`,
/// `user[profile]`).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RemoteSubscribeBody {
    /// The submitted pair.
    user: Option<SubscribeFields>,
}

/// The nested `user` map of [`RemoteSubscribeBody`].
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SubscribeFields {
    /// The local account.
    nickname: Option<String>,
    /// The remote handle.
    profile: Option<String>,
}

/// `POST /api/v1/pleroma/remote_interaction` (Pleroma's
/// `RemoteInteractionController.remote_interaction`): the JSON answer of
/// the subscribe-template lookup.
///
/// # Errors
///
/// [`AppError::BadRequest`] with [`MISSING_AP_ID`] or [`MISSING_PROFILE`]
/// for an absent or blank parameter, and with [`COULD_NOT_FIND_USER`]
/// when `profile` resolves through `WebFinger` to no usable subscribe
/// template.
async fn remote_interaction(
    State(state): State<AppState>,
    ApiBody(params): ApiBody<InteractionParams>,
) -> AppResult<Json<InteractionReply>> {
    interaction_answer(&state, params, None).await
}

/// The `POST /api/v1/pleroma/remote_interaction` decision, extracted so
/// the loopback-mock tests can point `webfinger_base` at their `WebFinger`
/// answer (the seam of [`crate::api::search`]' resolver and
/// [`crate::federation::webfinger_client`]'s reader). Production reaches
/// it only through [`remote_interaction`], which passes no override.
async fn interaction_answer(
    state: &AppState,
    params: InteractionParams,
    webfinger_base: Option<&str>,
) -> AppResult<Json<InteractionReply>> {
    let Some(ap_id) = required(params.ap_id.as_deref()) else {
        return Err(AppError::BadRequest(MISSING_AP_ID.to_owned()));
    };
    let Some(profile) = required(params.profile.as_deref()) else {
        return Err(AppError::BadRequest(MISSING_PROFILE.to_owned()));
    };
    let url = subscribe_url_for(&state.http_client, ap_id, profile, webfinger_base)
        .await
        .ok_or_else(|| AppError::BadRequest(COULD_NOT_FIND_USER.to_owned()))?;
    Ok(Json(InteractionReply { url }))
}

/// The whole subscribe lookup shared by the JSON endpoint and
/// `POST /main/ostatus`: `WebFinger`s `profile` and expands `target` into
/// its subscribe template. `webfinger_base` replaces the spec-fixed
/// `https://{domain}` origin of the lookup — [`None`] in production, a
/// loopback mock's origin in the tests (the override
/// [`crate::federation::webfinger_client`] gives its resolver). Failures
/// are logged, never rendered.
///
/// [`None`] for an unresolvable profile, a document with no
/// `rel=http://ostatus.org/schema/1.0/subscribe` link, and one whose
/// template carries no macro to expand.
async fn subscribe_url_for(
    http: &HttpClient,
    target: &str,
    profile: &str,
    webfinger_base: Option<&str>,
) -> Option<String> {
    match finger_with_base(http, profile, webfinger_base).await {
        Ok(document) => expand_subscribe_url(&document, target),
        Err(err) => {
            debug!(%err, profile, "webfinger found no answer for the subscribe profile");
            None
        }
    }
}

/// The subscribe link's template of a `WebFinger` document: the
/// `template` of the first `links` entry whose `rel` (trimmed) names
/// [`SUBSCRIBE_REL`] and that carries a non-blank template. A `rel` or
/// `template` that is not a string is as absent as a blank one.
fn subscribe_template(document: &Value) -> Option<&str> {
    document
        .get("links")
        .and_then(Value::as_array)?
        .iter()
        .find_map(|link| {
            let rel = link.get("rel").and_then(Value::as_str)?.trim();
            if rel != SUBSCRIBE_REL {
                return None;
            }
            let template = link.get("template").and_then(Value::as_str)?.trim();
            (!template.is_empty()).then_some(template)
        })
}

/// The subscribe URL: the document's template with `target`
/// percent-encoded into its macro. Both spellings the `WebFinger` client
/// accepts for the LRDD macro are expanded (`{uri}` and `%7Buri%7D`); a
/// template with neither cannot carry the target, and is no URL. The
/// encoding is [`url::form_urlencoded`]'s serializer: everything outside
/// `[A-Za-z0-9*-._]` becomes its `%XX` UTF-8 bytes, so a full `ap_id`
/// URL survives being quoted into another URL's query value intact.
fn expand_subscribe_url(document: &Value, target: &str) -> Option<String> {
    let template = subscribe_template(document)?;
    let encoded: String = form_urlencoded::byte_serialize(target.as_bytes()).collect();
    if template.contains(URI_MACRO) {
        return Some(template.replace(URI_MACRO, &encoded));
    }
    if template.contains(URI_MACRO_ENCODED) {
        return Some(template.replace(URI_MACRO_ENCODED, &encoded));
    }
    None
}

/// `GET /ostatus_subscribe` (Pleroma's `RemoteInteractionController.follow`
/// in its form half): the login form that follows `acct`, with every
/// value escaped. The page itself resolves nothing — an unknown handle
/// is the `POST`'s re-rendered error, not this route's.
async fn subscribe_form(Query(query): Query<SubscribeQuery>) -> Response {
    subscribe_page(trimmed(query.acct.as_deref()), None)
}

/// `POST /ostatus_subscribe` (Pleroma's `do_follow` without its MFA
/// steps): authenticate the named local user, resolve the `acct` and
/// record the follow. The success page follows a completed follow; every
/// failure re-renders the form with an error line and the submitted
/// `acct` kept.
async fn do_subscribe(
    State(state): State<AppState>,
    ApiBody(form): ApiBody<SubscribeSubmit>,
) -> Response {
    let acct = trimmed(form.acct.as_deref());
    match attempt_subscribe(&state, &form).await {
        Ok(followed) => success_page(&followed),
        Err(error) => subscribe_page(acct, Some(error.as_str())),
    }
}

/// The `POST /ostatus_subscribe` flow, as the outcome the route renders:
/// the followed `acct` on success, the page-level error line otherwise —
/// Pleroma's three buckets (bad login, unknown user, something went
/// wrong) with the follow model's own wording for the refusals it states
/// for the submitter to read.
async fn attempt_subscribe(state: &AppState, form: &SubscribeSubmit) -> Result<String, String> {
    let Some(acct) = required(form.acct.as_deref()) else {
        return Err(COULD_NOT_FIND_USER_PAGE.to_owned());
    };
    let name = form
        .authorization
        .as_ref()
        .and_then(|authorization| required(authorization.name.as_deref()));
    let password = form
        .authorization
        .as_ref()
        .and_then(|authorization| authorization.password.as_deref())
        .filter(|password| !password.is_empty());
    let (Some(name), Some(password)) = (name, password) else {
        return Err(WRONG_CREDENTIALS.to_owned());
    };
    let Some(user) = authenticate(&state.db, name, password).await else {
        return Err(WRONG_CREDENTIALS.to_owned());
    };
    let Some(target) = resolve_subscribe_target(state, acct).await else {
        return Err(COULD_NOT_FIND_USER_PAGE.to_owned());
    };
    match follow::follow(&state.db, &state.config.base_url, &user, &target).await {
        Ok(created) => {
            after_follow_changed(state, &user, &target, Some(&created), true).await;
            Ok(acct.to_owned())
        }
        Err(err) => Err(follow_page_error(&err)),
    }
}

/// The target of `POST /ostatus_subscribe`: the `acct` resolved by
/// [`get_or_fetch_by_acct`] (the database mirror first, `WebFinger` for
/// an unseen handle), live accounts only. A refusal or a deactivated row
/// is the shared [`None`] — the caller renders no detail beyond
/// [`COULD_NOT_FIND_USER_PAGE`].
async fn resolve_subscribe_target(state: &AppState, acct: &str) -> Option<User> {
    match get_or_fetch_by_acct(state, acct).await {
        Ok(target) if target.is_active.get() => Some(target),
        Ok(target) => {
            debug!(nickname = %target.nickname, "a subscribe target without a live row");
            None
        }
        Err(err) => {
            debug!(%err, acct, "the subscribe target could not be resolved");
            None
        }
    }
}

/// The error line a refused follow renders: the model's own client-safe
/// wording for a refusal it means the submitter to read (a self-follow,
/// a block), the catch-all for everything else — a database failure's
/// detail stays in the log (decision 8).
fn follow_page_error(err: &AppError) -> String {
    if matches!(
        err,
        AppError::Unprocessable(_)
            | AppError::Forbidden(_)
            | AppError::BadRequest(_)
            | AppError::Conflict(_)
    ) {
        return err.to_string();
    }
    debug!(%err, "the subscribe follow write failed");
    SOMETHING_WENT_WRONG.to_owned()
}

/// `GET /authorize_interaction` (Pleroma's `authorize_interaction`,
/// which bounces to its own subscribe form; this instance keeps the page
/// flow on the frontend instead): the 302 to the frontend page for
/// whatever `uri` resolves to.
///
/// # Errors
///
/// [`AppError::NotFound`] — the 404 JSON — for an absent or blank `uri`
/// and for one that resolves to no actor and no status.
async fn authorize_interaction(
    State(state): State<AppState>,
    Query(query): Query<InteractionQuery>,
) -> AppResult<Response> {
    let Some(uri) = required(query.uri.as_deref()) else {
        return Err(AppError::NotFound);
    };
    interaction_redirect(&state, uri).await
}

/// The `GET /authorize_interaction` resolution rule: an `http(s)` URL is
/// tried as an actor first and as a status second (Pleroma's own check
/// distinguishes the same two shapes); anything else is a handle through
/// [`get_or_fetch_by_acct`] with its `@`/`acct:` prefixes folded. A local
/// row answers from the database in either direction; only an unseen
/// remote address touches the network.
async fn interaction_redirect(state: &AppState, uri: &str) -> AppResult<Response> {
    if looks_like_ap_id(uri) {
        if let Ok(user) = get_or_fetch_by_ap_id(state, uri).await {
            return frontend_redirect(&state.config, &format!("users/{}", user.id));
        }
        if let Ok(Some(status)) = resolve_status(state, uri).await {
            return frontend_redirect(&state.config, &format!("notice/{}", status.id));
        }
        debug!(uri, "authorize_interaction resolved no actor and no status");
        return Err(AppError::NotFound);
    }
    match get_or_fetch_by_acct(state, strip_finger_prefixes(uri)).await {
        Ok(user) => frontend_redirect(&state.config, &format!("users/{}", user.id)),
        Err(err) => {
            debug!(%err, uri, "authorize_interaction could not resolve the handle");
            Err(AppError::NotFound)
        }
    }
}

/// `GET /main/ostatus` (Pleroma's `show_subscribe_form` in its form
/// half): the remote-subscribe form, prefilled from the flat `nickname`
/// and `profile` query pair and escaped.
async fn show_remote_subscribe(Query(query): Query<RemoteSubscribeQuery>) -> Response {
    remote_subscribe_page(
        trimmed(query.nickname.as_deref()),
        trimmed(query.profile.as_deref()),
        None,
    )
}

/// `POST /main/ostatus` (Pleroma's `remote_subscribe` with the nested
/// `user` map): the local actor named by `user[nickname]`, the subscribe
/// template of `user[profile]`, and the 302 to the URL the JSON endpoint
/// would have answered for the pair. Every failure re-renders the form
/// with an error line and both submitted values kept.
async fn do_remote_subscribe(
    State(state): State<AppState>,
    ApiBody(body): ApiBody<RemoteSubscribeBody>,
) -> Response {
    let empty = SubscribeFields::default();
    let fields = body.user.as_ref().unwrap_or(&empty);
    remote_subscribe_answer(
        &state,
        trimmed(fields.nickname.as_deref()),
        trimmed(fields.profile.as_deref()),
        None,
    )
    .await
}

/// The `POST /main/ostatus` decision, extracted so the loopback-mock
/// tests can point `webfinger_base` at their `WebFinger` answer (the same
/// seam as [`interaction_answer`]). Empty fields are the "no such user"
/// refusal; the local actor's `ap_id` expanded into the profile's
/// subscribe template is the redirect; anything that fails is the form
/// back with an error line.
async fn remote_subscribe_answer(
    state: &AppState,
    nickname: &str,
    profile: &str,
    webfinger_base: Option<&str>,
) -> Response {
    if nickname.is_empty() || profile.is_empty() {
        return remote_subscribe_page(nickname, profile, Some(COULD_NOT_FIND_USER_PAGE));
    }
    let Some(user) = local_subscribe_actor(state, nickname).await else {
        return remote_subscribe_page(nickname, profile, Some(COULD_NOT_FIND_USER_PAGE));
    };
    let Some(url) =
        subscribe_url_for(&state.http_client, &user.ap_id, profile, webfinger_base).await
    else {
        return remote_subscribe_page(nickname, profile, Some(SOMETHING_WENT_WRONG));
    };
    found_redirect(&url)
        .unwrap_or_else(|| remote_subscribe_page(nickname, profile, Some(SOMETHING_WENT_WRONG)))
}

/// The `user[nickname]` of `POST /main/ostatus`: the row
/// [`get_by_acct`] finds for the submitted handle — bare nickname or
/// spelled with this instance's own domain alike — when it is a live
/// *local* account. This page sends the local actor's `ap_id` out, so a
/// remote mirror is no actor to interact as.
async fn local_subscribe_actor(state: &AppState, nickname: &str) -> Option<User> {
    let found = match get_by_acct(&state.db, &state.config, nickname).await {
        Ok(found) => found,
        Err(err) => {
            debug!(%err, nickname, "the subscribe actor could not be read");
            return None;
        }
    };
    found.filter(|user| user.local.get() && user.is_active.get())
}

/// The 302 to `{FRONTEND_BASE_URL}/{path}` (the shape
/// [`crate::api::activitypub`]'s frontend redirects share): a location
/// whose text cannot ride a `Location` header names no page, and the
/// route answers its own 404.
fn frontend_redirect(config: &Config, path: &str) -> AppResult<Response> {
    let location = format!("{}/{}", config.frontend_base_url, path);
    let value = HeaderValue::from_str(&location)
        .ok()
        .ok_or(AppError::NotFound)?;
    Ok((StatusCode::FOUND, [(header::LOCATION, value)]).into_response())
}

/// The external 302 of `POST /main/ostatus`: the `Location` is a remote
/// instance's URL, so one that is not header-safe is [`None`] and the
/// caller re-renders the form.
fn found_redirect(location: &str) -> Option<Response> {
    let value = HeaderValue::from_str(location).ok()?;
    Some((StatusCode::FOUND, [(header::LOCATION, value)]).into_response())
}

/// The `GET/POST /ostatus_subscribe` page: [`subscribe_body`] under the
/// module headers.
fn subscribe_page(acct: &str, error: Option<&str>) -> Response {
    page(FOLLOW_TITLE, &subscribe_body(acct, error))
}

/// The body of the login page: the message, the optional error line, and
/// the form posting the hidden `acct` plus the nested credentials to
/// [`SUBSCRIBE_PATH`]. Every request-sourced value goes through
/// [`escape_html`].
fn subscribe_body(acct: &str, error: Option<&str>) -> String {
    let line = error_line(error);
    let escaped_acct = escape_html(acct);
    format!(
        "<h1>{FOLLOW_TITLE}</h1>\n{line}\
         <form method=\"post\" action=\"{SUBSCRIBE_PATH}\">\n\
         <input type=\"hidden\" name=\"acct\" value=\"{escaped_acct}\">\n\
         <label for=\"authorization_name\">Username</label>\n\
         <input type=\"text\" id=\"authorization_name\" name=\"authorization[name]\" \
         autocomplete=\"username\" required>\n\
         <label for=\"authorization_password\">Password</label>\n\
         <input type=\"password\" id=\"authorization_password\" \
         name=\"authorization[password]\" autocomplete=\"current-password\" required>\n\
         <input type=\"submit\" value=\"Follow\">\n\
         </form>\n",
    )
}

/// The page a completed follow answers with: [`success_body`] under the
/// module headers.
fn success_page(acct: &str) -> Response {
    page(FOLLOW_TITLE, &success_body(acct))
}

/// The body of the success page: the followed handle, escaped.
fn success_body(acct: &str) -> String {
    let escaped_acct = escape_html(acct);
    format!("<h1>{FOLLOW_TITLE}</h1>\n<p>{FOLLOWED_MESSAGE} <b>{escaped_acct}</b>.</p>\n")
}

/// The `GET/POST /main/ostatus` page: [`remote_subscribe_body`] under the
/// module headers.
fn remote_subscribe_page(nickname: &str, profile: &str, error: Option<&str>) -> Response {
    page(
        REMOTE_SUBSCRIBE_TITLE,
        &remote_subscribe_body(nickname, profile, error),
    )
}

/// The body of the remote-subscribe form: the message, the optional
/// error line, and the form posting `user[nickname]` / `user[profile]`
/// to [`REMOTE_SUBSCRIBE_PATH`], both values escaped.
fn remote_subscribe_body(nickname: &str, profile: &str, error: Option<&str>) -> String {
    let line = error_line(error);
    let escaped_nickname = escape_html(nickname);
    let escaped_profile = escape_html(profile);
    format!(
        "<h1>{REMOTE_SUBSCRIBE_TITLE}</h1>\n{line}\
         <form method=\"post\" action=\"{REMOTE_SUBSCRIBE_PATH}\">\n\
         <label for=\"user_nickname\">Your account</label>\n\
         <input type=\"text\" id=\"user_nickname\" name=\"user[nickname]\" \
         value=\"{escaped_nickname}\" required>\n\
         <label for=\"user_profile\">Remote account</label>\n\
         <input type=\"text\" id=\"user_profile\" name=\"user[profile]\" \
         value=\"{escaped_profile}\" required>\n\
         <input type=\"submit\" value=\"Subscribe\">\n\
         </form>\n",
    )
}

/// The error line of a re-rendered page, the empty string for a first
/// render: the escaped text inside `<p class="error">`.
fn error_line(error: Option<&str>) -> String {
    let Some(text) = error else {
        return String::new();
    };
    let escaped = escape_html(text);
    format!("<p class=\"error\">{escaped}</p>\n")
}

/// A page response: [`page_document`]'s HTML under the module headers —
/// the HTML media type, [`PAGE_CSP`] and `X-Frame-Options: DENY`.
fn page(title: &str, body: &str) -> Response {
    let mut response = (StatusCode::OK, page_document(title, body)).into_response();
    put_header(&mut response, header::CONTENT_TYPE, PAGE_CONTENT_TYPE);
    put_header(&mut response, CSP_HEADER.clone(), PAGE_CSP);
    put_header(&mut response, FRAME_OPTIONS_HEADER.clone(), "DENY");
    response
}

/// The full document of a page: the doctype, the escaped `title`, the
/// one inline [`PAGE_STYLE`] sheet and `body` inside `<main>`. The body
/// is markup this module's builders wrote, already escaped where it came
/// from a request.
fn page_document(title: &str, body: &str) -> String {
    let escaped_title = escape_html(title);
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{escaped_title}</title>\n\
         <style>{PAGE_STYLE}</style>\n\
         </head>\n<body>\n<main>\n{body}\n</main>\n</body>\n</html>\n",
    )
}

/// Sets header `name` on `response` to the static value `value`,
/// replacing any earlier one (the old value may not be dropped silently).
fn put_header(response: &mut Response, name: HeaderName, value: &'static str) {
    let replaced = response
        .headers_mut()
        .insert(name, HeaderValue::from_static(value));
    drop(replaced);
}

/// `value` trimmed, or the empty string when the request carried none.
fn trimmed(value: Option<&str>) -> &str {
    value.map_or("", str::trim)
}

/// The parameter `value` when the request carried it non-blank after
/// trimming — the presence rule of every required parameter here.
fn required(value: Option<&str>) -> Option<&str> {
    let text = trimmed(value);
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use axum::{Router, http::StatusCode, routing::get};
    use serde_json::json;
    use sqlx::PgPool;
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        federation::webfinger_client::{JRD_JSON, XRD_XML},
        models::user::{NewUser, insert, local_ap_id},
        uploads::UploaderBackend,
    };

    /// `User-Agent` for the test client.
    const TEST_UA: &str = "Wired (https://test.example)";
    /// The local actor URL the tests expand into the templates.
    const AP_ID: &str = "https://test.example/users/alice";
    /// [`AP_ID`] as [`expand_subscribe_url`] encodes it.
    const AP_ID_ENCODED: &str = "https%3A%2F%2Ftest.example%2Fusers%2Falice";
    /// The remote handle every mock answers about.
    const PROFILE: &str = "bob@remote.example";
    /// The interaction URL of the [`JRD_SUBSCRIBE`] mock.
    const WANT_URL: &str =
        "https://remote.example/ostatus_subscribe?acct=https%3A%2F%2Ftest.example%2Fusers%2Falice";
    /// A JRD answer advertising a subscribe template for [`PROFILE`].
    const JRD_SUBSCRIBE: &str = r#"{"subject": "acct:bob@remote.example", "links": [
        {"rel": "self", "type": "application/activity+json",
         "href": "https://remote.example/users/bob"},
        {"rel": "http://ostatus.org/schema/1.0/subscribe",
         "template": "https://remote.example/ostatus_subscribe?acct={uri}"}
    ]}"#;
    /// The same answer with the percent-encoded macro spelling.
    const JRD_ENCODED_MACRO: &str = r#"{"links": [
        {"rel": "http://ostatus.org/schema/1.0/subscribe",
         "template": "https://remote.example/follow?uri=%7Buri%7D"}
    ]}"#;
    /// An XRD answer advertising a subscribe template.
    const XRD_SUBSCRIBE: &str = r#"<XRD xmlns="http://docs.oasis-open.org/ns/xri/xrd-1.0">
        <Subject>acct:bob@remote.example</Subject>
        <Link rel="http://ostatus.org/schema/1.0/subscribe"
              template="https://remote.example/ostatus_subscribe?acct={uri}"/>
    </XRD>"#;
    /// An answer whose subscribe link carries no template.
    const JRD_NO_TEMPLATE: &str = r#"{"links": [
        {"rel": "http://ostatus.org/schema/1.0/subscribe"}
    ]}"#;
    /// An answer with a subscribe link whose template has no macro.
    const JRD_NO_MACRO: &str = r#"{"links": [
        {"rel": "http://ostatus.org/schema/1.0/subscribe",
         "template": "https://remote.example/follow"}
    ]}"#;
    /// An answer with no subscribe link at all.
    const JRD_NO_SUBSCRIBE: &str = r#"{"links": [
        {"rel": "self", "type": "application/activity+json",
         "href": "https://remote.example/users/bob"}
    ]}"#;

    /// The client under test: the SSRF guard off, so loopback mocks are
    /// reachable (the mode `HTTP_ALLOW_PRIVATE_HOSTS` selects).
    fn client() -> HttpClient {
        HttpClient::new_for_tests(TEST_UA)
    }

    /// A router answering `/.well-known/webfinger` with `body`, labelled
    /// `content_type`.
    fn webfinger_route(content_type: &'static str, body: &'static str) -> Router {
        /// Answer with the configured document.
        async fn handler(State((content_type, body)): State<(String, String)>) -> Response {
            ([(header::CONTENT_TYPE, content_type)], body).into_response()
        }
        Router::new()
            .route("/.well-known/webfinger", get(handler))
            .with_state((content_type.to_owned(), body.to_owned()))
    }

    /// A router answering every path with `status`.
    fn status_route(status: StatusCode) -> Router {
        /// Reply with the configured status.
        async fn handler(State(status): State<StatusCode>) -> StatusCode {
            status
        }
        Router::new()
            .route("/{*rest}", get(handler))
            .with_state(status)
    }

    /// Serves `app` on a loopback port; returns its task handle (to abort
    /// when the test is done) and the `http://` base URL to hand the
    /// lookup as the scheme-and-authority override.
    async fn serve(app: Router) -> (JoinHandle<()>, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("test server stopped: {err}");
            }
        });
        (server, base)
    }

    /// The subscribe URL `target` expands to against `app`'s answer for
    /// [`PROFILE`], served on loopback.
    async fn url_against(app: Router, target: &str) -> Option<String> {
        let (server, base) = serve(app).await;
        let url = subscribe_url_for(&client(), target, PROFILE, Some(&base)).await;
        server.abort();
        url
    }

    /// Test state over `pool`: the fixed test config with a client whose
    /// SSRF guard is off, so the subscribe lookup may reach the loopback
    /// mock (the shape [`crate::api::search`]'s tests build).
    fn test_state(pool: &PgPool) -> AppState {
        let config = std::sync::Arc::new(Config::for_tests());
        AppState {
            db: pool.clone(),
            hub: crate::streaming::hub::Hub::new(pool),
            config: std::sync::Arc::clone(&config),
            http_client: HttpClient::new_for_tests(TEST_UA),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: std::sync::Arc::default(),
            mrf_config_cache: std::sync::Arc::default(),
            translation_languages_cache: std::sync::Arc::default(),
            rate_limiter: std::sync::Arc::new(crate::rate_limit::RateLimiter::from_config(
                config.as_ref(),
            )),
            mailer: std::sync::Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// A state over a pool that never connects: the JSON endpoint reads
    /// only the HTTP client, so no slot is spent on it.
    fn client_state() -> AppState {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .unwrap();
        test_state(&pool)
    }

    /// The local `alice` whose actor id is [`AP_ID`], inserted straight.
    async fn local_alice(pool: &PgPool) -> User {
        insert(
            pool,
            &NewUser::local("alice", &local_ap_id(TEST_BASE_URL, "alice")),
        )
        .await
        .expect("the fixture local user inserts")
    }

    /// The full text of a response body.
    async fn body_of(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn the_json_endpoint_answers_the_interaction_url() {
        let (server, base) = serve(webfinger_route(JRD_JSON, JRD_SUBSCRIBE)).await;
        let state = client_state();
        let params = InteractionParams {
            ap_id: Some(AP_ID.to_owned()),
            profile: Some(PROFILE.to_owned()),
        };
        let reply = interaction_answer(&state, params, Some(&base))
            .await
            .expect("the mock answers a subscribe template");
        server.abort();
        let response = reply.into_response();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the interaction answer is a 200"
        );
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/json",
            "and it is JSON"
        );
        assert_eq!(
            body_of(response).await,
            format!(r#"{{"url":"{WANT_URL}"}}"#),
            "the exact body the ticket fixes"
        );
    }

    #[tokio::test]
    async fn the_json_endpoint_refuses_without_the_network() {
        let state = client_state();
        for (params, why, want) in [
            (
                InteractionParams {
                    ap_id: None,
                    profile: Some(PROFILE.to_owned()),
                },
                "an absent ap_id",
                MISSING_AP_ID,
            ),
            (
                InteractionParams {
                    ap_id: Some("   ".to_owned()),
                    profile: Some(PROFILE.to_owned()),
                },
                "a blank ap_id",
                MISSING_AP_ID,
            ),
            (
                InteractionParams {
                    ap_id: Some(AP_ID.to_owned()),
                    profile: None,
                },
                "an absent profile",
                MISSING_PROFILE,
            ),
            (
                InteractionParams {
                    ap_id: Some(AP_ID.to_owned()),
                    profile: Some("bob@invalid".to_owned()),
                },
                "an unusable profile (no request ever leaves)",
                COULD_NOT_FIND_USER,
            ),
        ] {
            let err = interaction_answer(&state, params, None)
                .await
                .expect_err(&format!("{why} must be refused"));
            assert!(matches!(err, AppError::BadRequest(_)), "{why} is a 400");
            assert_eq!(err.to_string(), want, "{why}: the exact message");
        }
    }

    #[tokio::test]
    async fn the_json_endpoint_refuses_a_profile_without_a_subscribe_link() {
        let (server, base) = serve(webfinger_route(JRD_JSON, JRD_NO_SUBSCRIBE)).await;
        let state = client_state();
        let params = InteractionParams {
            ap_id: Some(AP_ID.to_owned()),
            profile: Some(PROFILE.to_owned()),
        };
        let err = interaction_answer(&state, params, Some(&base))
            .await
            .expect_err("no template is no interaction");
        server.abort();
        assert_eq!(
            err.to_string(),
            COULD_NOT_FIND_USER,
            "Pleroma's refusal wording for an answer with no subscribe link"
        );
    }

    #[tokio::test]
    async fn the_remote_subscribe_post_redirects_to_the_interaction_url() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = local_alice(&pool).await;
        assert_eq!(
            alice.ap_id, AP_ID,
            "the fixture actor is the expanded target"
        );
        let (server, base) = serve(webfinger_route(JRD_JSON, JRD_SUBSCRIBE)).await;

        let plain = remote_subscribe_answer(&state, "alice", PROFILE, Some(&base)).await;
        let spelled =
            remote_subscribe_answer(&state, "alice@test.example", PROFILE, Some(&base)).await;
        server.abort();

        for response in [plain, spelled] {
            assert_eq!(
                response.status(),
                StatusCode::FOUND,
                "the subscribe post redirects"
            );
            assert_eq!(
                response.headers()[header::LOCATION],
                WANT_URL,
                "to the same url the JSON endpoint answers"
            );
        }
    }

    #[tokio::test]
    async fn the_remote_subscribe_post_re_renders_when_the_profile_answers_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let _alice: User = local_alice(&pool).await;
        let (server, base) = serve(webfinger_route(JRD_JSON, JRD_NO_TEMPLATE)).await;

        let response = remote_subscribe_answer(&state, "alice", PROFILE, Some(&base)).await;
        server.abort();

        assert_eq!(response.status(), StatusCode::OK, "the form comes back");
        let body = body_of(response).await;
        assert!(
            body.contains("<p class=\"error\">Something went wrong.</p>"),
            "Pleroma's catch-all line: {body}"
        );
        assert!(body.contains("value=\"alice\""), "the pair is kept: {body}");
    }

    #[tokio::test]
    async fn the_remote_subscribe_post_refuses_a_nonlocal_actor_without_a_lookup() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let eve: User = insert(
            &pool,
            &NewUser {
                nickname: "eve@other.example".to_owned(),
                ap_id: "https://other.example/users/eve".to_owned(),
                domain: Some("other.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the fixture mirror inserts");
        assert!(!eve.local.get(), "the fixture is a remote mirror");

        for nickname in ["nobody", "eve@other.example"] {
            let response = remote_subscribe_answer(&state, nickname, "bob@invalid", None).await;
            assert_eq!(response.status(), StatusCode::OK, "the form comes back");
            let body = body_of(response).await;
            assert!(
                body.contains("<p class=\"error\">Could not find user</p>"),
                "`{nickname}` is no local actor to interact as: {body}"
            );
        }
    }

    #[test]
    fn the_subscribe_url_expands_the_macro_with_the_encoded_target() {
        let document = json!({"links": [
            {"rel": SUBSCRIBE_REL, "template": "https://r.example/f?acct={uri}"}
        ]});
        assert_eq!(
            expand_subscribe_url(&document, AP_ID).as_deref(),
            Some("https://r.example/f?acct=https%3A%2F%2Ftest.example%2Fusers%2Falice"),
            "the {{uri}} macro takes the percent-encoded ap_id"
        );
    }

    #[test]
    fn the_percent_encoded_macro_is_expanded_too() {
        let document = json!({"links": [
            {"rel": SUBSCRIBE_REL, "template": "https://r.example/f?acct=%7Buri%7D"}
        ]});
        assert_eq!(
            expand_subscribe_url(&document, AP_ID).as_deref(),
            Some(format!("https://r.example/f?acct={AP_ID_ENCODED}").as_str()),
            "both macro spellings carry the target"
        );
    }

    #[test]
    fn the_encoded_target_cannot_break_out_of_the_query_value() {
        let document = json!({"links": [
            {"rel": SUBSCRIBE_REL, "template": "https://r.example/f?u={uri}"}
        ]});
        let url = expand_subscribe_url(&document, "https://x.example/a b&c=1").expect("a url");
        assert_eq!(
            url, "https://r.example/f?u=https%3A%2F%2Fx.example%2Fa+b%26c%3D1",
            "space, & and = are all encoded into the value"
        );
    }

    #[test]
    fn only_the_subscribe_rel_with_a_template_counts() {
        for (document, why) in [
            (json!({"links": []}), "no links at all"),
            (
                json!({"links": [{"rel": "self", "template": "https://x/?u={uri}"}]}),
                "the wrong rel",
            ),
            (
                json!({"links": [{"rel": "http://webfinger.net/rel/profile-page"}]}),
                "no subscribe link",
            ),
            (
                json!({"links": [{"rel": SUBSCRIBE_REL, "template": "  "}]}),
                "a blank template",
            ),
            (
                json!({"links": [{"rel": SUBSCRIBE_REL, "template": "https://x/f"}]}),
                "a template with no macro",
            ),
            (json!({"links": "not-an-array"}), "links that are no array"),
            (json!({}), "no links member"),
        ] {
            assert_eq!(
                expand_subscribe_url(&document, AP_ID),
                None,
                "{why} is no interaction url"
            );
        }
    }

    #[test]
    fn the_first_usable_subscribe_link_wins() {
        let document = json!({"links": [
            {"rel": SUBSCRIBE_REL, "template": "https://first.example/f?u={uri}"},
            {"rel": SUBSCRIBE_REL, "template": "https://second.example/f?u={uri}"}
        ]});
        assert_eq!(
            expand_subscribe_url(&document, AP_ID).as_deref(),
            Some("https://first.example/f?u=https%3A%2F%2Ftest.example%2Fusers%2Falice"),
            "document order decides"
        );
    }

    #[test]
    fn the_subscribe_rel_is_trimmed_before_comparing() {
        let document = json!({"links": [
            {"rel": format!(" {SUBSCRIBE_REL} "), "template": "https://r.example/f?u={uri}"}
        ]});
        assert!(
            expand_subscribe_url(&document, AP_ID).is_some(),
            "surrounding whitespace is not part of the rel"
        );
    }

    #[tokio::test]
    async fn a_jrd_answer_builds_the_interaction_url() {
        let app = webfinger_route(JRD_JSON, JRD_SUBSCRIBE);
        assert_eq!(
            url_against(app, AP_ID).await.as_deref(),
            Some(WANT_URL),
            "the template with the encoded ap_id in place of the macro"
        );
    }

    #[tokio::test]
    async fn an_xrd_answer_builds_the_same_url() {
        let app = webfinger_route(XRD_XML, XRD_SUBSCRIBE);
        assert_eq!(
            url_against(app, AP_ID).await.as_deref(),
            Some(WANT_URL),
            "the normalized document reads the same"
        );
    }

    #[tokio::test]
    async fn the_encoded_macro_answer_builds_the_url_too() {
        let app = webfinger_route(JRD_JSON, JRD_ENCODED_MACRO);
        assert_eq!(
            url_against(app, AP_ID).await.as_deref(),
            Some(format!("https://remote.example/follow?uri={AP_ID_ENCODED}").as_str()),
            "the %7Buri%7D spelling expands"
        );
    }

    #[tokio::test]
    async fn a_useless_subscribe_answer_is_no_url() {
        for (body, why) in [
            (JRD_NO_TEMPLATE, "the link carries no template"),
            (JRD_NO_MACRO, "the template carries no macro"),
            (JRD_NO_SUBSCRIBE, "there is no subscribe link"),
            ("{oops", "the body is no document"),
        ] {
            let app = webfinger_route(JRD_JSON, body);
            assert_eq!(
                url_against(app, AP_ID).await,
                None,
                "{why} is no interaction url"
            );
        }
    }

    #[tokio::test]
    async fn an_unreachable_profile_is_no_url() {
        let app = status_route(StatusCode::NOT_FOUND);
        assert_eq!(
            url_against(app, AP_ID).await,
            None,
            "a 404 WebFinger with no host-meta fallback resolves no profile"
        );
    }

    #[tokio::test]
    async fn an_unusable_profile_never_reaches_the_network() {
        // No server at all: `bob@invalid` fails the handle grammar before
        // any request, so this proves the early refusal.
        let missing = subscribe_url_for(&client(), AP_ID, "bob@invalid", None).await;
        assert_eq!(missing, None, "a single-label domain resolves nothing");
    }

    #[test]
    fn the_subscribe_form_posts_the_documented_fields() {
        let body = subscribe_body(r#"<i>&"'x"#, Some("bad & worse"));
        assert!(
            body.contains("action=\"/ostatus_subscribe\""),
            "the form posts back to the subscribe route: {body}"
        );
        assert!(
            body.contains("name=\"acct\" value=\"&lt;i&gt;&amp;&quot;&#39;x\""),
            "the hidden acct is escaped: {body}"
        );
        assert!(
            body.contains("name=\"authorization[name]\""),
            "the username field is the nested authorization name: {body}"
        );
        assert!(
            body.contains("name=\"authorization[password]\""),
            "the password field is the nested authorization password: {body}"
        );
        assert!(
            body.contains("type=\"password\""),
            "the password input hides its text: {body}"
        );
        assert!(
            body.contains("<p class=\"error\">bad &amp; worse</p>"),
            "the error line is escaped: {body}"
        );
    }

    #[test]
    fn a_first_render_shows_no_error_line() {
        let body = subscribe_body("bob@remote.example", None);
        assert!(
            !body.contains("class=\"error\""),
            "no failure, no line: {body}"
        );
    }

    #[test]
    fn the_remote_subscribe_form_carries_both_nested_fields() {
        let body = remote_subscribe_body(
            "alice\" onload=\"pwn()\"",
            "<b>bob@remote.example</b>",
            Some(SOMETHING_WENT_WRONG),
        );
        assert!(
            body.contains("action=\"/main/ostatus\""),
            "the form posts to the subscribe route: {body}"
        );
        assert!(
            body.contains("name=\"user[nickname]\" value=\"alice&quot; onload=&quot;pwn()&quot;\""),
            "the local account is escaped in its value: {body}"
        );
        assert!(
            body.contains("name=\"user[profile]\" value=\"&lt;b&gt;bob@remote.example&lt;/b&gt;\""),
            "the remote handle is escaped in its value: {body}"
        );
        assert!(
            body.contains("<p class=\"error\">Something went wrong.</p>"),
            "the failure says so: {body}"
        );
        assert!(
            !body.contains("<b>bob"),
            "no tag from the request survives: {body}"
        );
    }

    #[test]
    fn the_success_page_names_the_followed_acct_escaped() {
        let body = success_body(r#"eve'"><script>x</script>"#);
        assert!(body.contains(FOLLOWED_MESSAGE), "the success line");
        assert!(
            body.contains("<b>eve&#39;&quot;&gt;&lt;script&gt;x&lt;/script&gt;</b>"),
            "the acct arrives escaped: {body}"
        );
        assert!(!body.contains("<script"), "no tag opens anywhere: {body}");
    }

    #[test]
    fn the_page_document_escapes_its_title() {
        let html = page_document("<bad> & title", "<p>body</p>");
        assert!(html.starts_with("<!DOCTYPE html>"), "starts at the doctype");
        assert!(
            html.contains("<title>&lt;bad&gt; &amp; title</title>"),
            "the title is escaped: {html}"
        );
        assert!(
            html.contains("<main>\n<p>body</p>\n</main>"),
            "the body rides inside <main>: {html}"
        );
        assert!(!html.contains("<script"), "the pages carry no scripts");
    }

    #[test]
    fn every_page_answers_html_with_the_form_safe_policy() {
        for response in [
            subscribe_page("bob@remote.example", None),
            success_page("bob@remote.example"),
            remote_subscribe_page("alice", "bob@remote.example", None),
        ] {
            assert_eq!(response.status(), StatusCode::OK, "pages render at 200");
            let headers = response.headers();
            assert_eq!(
                headers[header::CONTENT_TYPE],
                PAGE_CONTENT_TYPE,
                "answered as HTML"
            );
            assert_eq!(
                headers[CSP_HEADER.clone()],
                PAGE_CSP,
                "the CSP allows the form's own submit"
            );
            assert!(
                PAGE_CSP.contains("form-action 'self'"),
                "and the policy names it: {PAGE_CSP}"
            );
            assert_eq!(
                headers[FRAME_OPTIONS_HEADER.clone()],
                "DENY",
                "a login form is not framable"
            );
        }
    }

    #[test]
    fn the_frontend_redirect_names_the_configured_origin() {
        let config = Config::for_tests();
        let response = frontend_redirect(&config, "users/01934568-1111-7000-8000-abcdef012345")
            .expect("a uuid path is header-safe");
        assert_eq!(response.status(), StatusCode::FOUND, "the redirect status");
        assert_eq!(
            response.headers()[header::LOCATION],
            "https://test.example/users/01934568-1111-7000-8000-abcdef012345",
            "the frontend origin carries the path"
        );
        let err = frontend_redirect(&config, "users/bad\nid")
            .expect_err("a path with a newline cannot ride a header");
        assert!(matches!(err, AppError::NotFound), "the 404 it answers with");
    }

    #[test]
    fn the_external_redirect_carries_the_remote_url_or_nothing() {
        let response = found_redirect("https://remote.example/ostatus_subscribe?acct=x")
            .expect("a plain url is header-safe");
        assert_eq!(response.status(), StatusCode::FOUND, "the redirect status");
        assert_eq!(
            response.headers()[header::LOCATION],
            "https://remote.example/ostatus_subscribe?acct=x",
            "the Location is the expanded template verbatim"
        );
        assert!(
            found_redirect("https://r.example/a\nb").is_none(),
            "a url with a newline is no redirect"
        );
    }

    #[test]
    fn required_parameters_are_present_and_non_blank() {
        assert_eq!(required(None), None, "an absent parameter");
        assert_eq!(required(Some("")), None, "an empty one");
        assert_eq!(required(Some("   ")), None, "a blank one");
        assert_eq!(
            required(Some(" https://x.example/a ")),
            Some("https://x.example/a"),
            "a value is trimmed"
        );
        assert_eq!(trimmed(None), "", "and an absent value reads as empty");
        assert_eq!(
            trimmed(Some(" https://x.example/a ")),
            "https://x.example/a"
        );
    }

    /// The routers mount every documented path.
    #[test]
    fn the_two_routers_carry_the_five_routes() {
        // Building them proves the paths are axum-legal; the handlers are
        // exercised over HTTP by the integration suite.
        let _pages: Router<AppState> = router();
        let _api: Router<AppState> = pleroma_router();
    }
}
