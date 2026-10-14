//! Frontend settings store, mascot and preferred frontend: the
//! `/api/v1/pleroma` client-settings surface (WBE-122).
//!
//! The reference is Pleroma's `SettingsController`
//! (`lib/pleroma/web/pleroma_api/controllers/settings_controller.ex`),
//! `MascotController`
//! (`lib/pleroma/web/pleroma_api/controllers/mascot_controller.ex`) and
//! `FrontendSettingsController`
//! (`lib/pleroma/web/pleroma_api/controllers/frontend_settings_controller.ex`).
//! The ticket shapes are authoritative where Pleroma's differ: the
//! settings-store size cap, the `app` name rule, the stored mascot (a
//! Pleroma `users.mascot` column; here a `mascot` key inside the settings
//! store) and the database-backed preferred frontend (Pleroma keeps the
//! choice in the cookie alone; here the cookie is set *and* the choice is
//! stored in `users.preferred_frontend`).
//!
//! Every route requires a user token ([`AuthUser`]).
//!
//! - `GET /api/v1/pleroma/settings/{app}` (scope `read:accounts`) — the
//!   `app` entry of the account's `settings_store` as an object; `{}` when
//!   it has none (or when the stored value is not an object, Pleroma's
//!   `ensure_object`). `{app}` is limited to 64 characters of
//!   `[A-Za-z0-9_.:-]`; anything else is 422 `{"error":"Invalid app"}`.
//! - `PATCH /api/v1/pleroma/settings/{app}` (scope `write:accounts`) — the
//!   body object is deep-merged into the stored settings (Pleroma's
//!   `merge_recursively`): a `null` value deletes that key, an object
//!   value merges recursively, anything else replaces. The merged object
//!   is the answer; the whole `settings_store` must stay within 1 MiB of
//!   JSON text, or the write is refused with 422
//!   `{"error":"settings_store: too large"}` and nothing changes.
//! - `GET /api/v1/pleroma/mascot` (scope `read:accounts`) — the account's
//!   mascot attachment (`settings_store["mascot"]`, the keys `id`, `url`,
//!   `type` and `pleroma.mime_type`) or the instance default:
//!   `{"id":"default-mascot","url":"{BASE_URL}/images/pleroma-fox-tan-smol.png",…}`,
//!   Pleroma's `[:assets, :default_mascot]` fox.
//! - `PUT /api/v1/pleroma/mascot` (scope `write:accounts`) — a multipart
//!   `file` part whose bytes [`infer`] names an `image/*` type; anything
//!   else is 415 `{"error":"mascots can only be images"}` (no bytes read
//!   from the client's `content-type` claim, as on the media upload). The
//!   file goes through [`crate::uploads`] and the attachment is stored
//!   under the `mascot` key of the settings store; the same attachment
//!   JSON is the answer.
//! - `GET /api/v1/pleroma/preferred_frontend/available` — the frontends
//!   the operator made pickable, `AVAILABLE_FRONTENDS` as a list
//!   (empty by default; Pleroma's `[:frontends, :pickable]`).
//! - `PUT /api/v1/pleroma/preferred_frontend` — body `frontend_name`,
//!   which must be one of the available frontends or the answer is 422
//!   `{"error":"Invalid frontend"}`. The choice is stored in the account's
//!   `preferred_frontend` column and echoed back in a year-lived cookie,
//!   `preferred_frontend=<name>; Path=/; Max-Age=31536000`; the answer is
//!   `{"frontend_name": name}`.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderValue, header},
    response::{IntoResponse as _, Response},
    routing::{get, put},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::{
    auth::AuthUser,
    config::Config,
    error::{AppError, AppResult},
    extract::{
        ApiBody,
        multipart::{MultipartForm, UploadedFile, UploadedFiles as _, upload_body_limit},
    },
    models::user,
    oauth::scopes::{READ_ACCOUNTS, WRITE_ACCOUNTS},
    state::AppState,
    uploads,
    util::id::new_id,
};

/// The whole settings store may not exceed one mebibyte of JSON text.
pub const MAX_SETTINGS_STORE_BYTES: usize = 1024 * 1024;
/// Longest accepted `{app}` path segment.
const MAX_APP_NAME_CHARS: usize = 64;
/// 422 body for a `{app}` segment outside [`MAX_APP_NAME_CHARS`] characters
/// of `[A-Za-z0-9_.:-]`.
const INVALID_APP: &str = "Invalid app";
/// 422 body for a merged settings store past [`MAX_SETTINGS_STORE_BYTES`],
/// the message the ticket fixes. Shared with
/// [`update_credentials`](crate::api::accounts), which writes the same
/// column through `pleroma_settings_store`.
pub const STORE_TOO_LARGE: &str = "settings_store: too large";
/// 415 body for mascot bytes that are not an image (Pleroma's
/// `MascotController` wording).
const MASCOTS_IMAGES_ONLY: &str = "mascots can only be images";
/// 422 body when the mascot upload carries no `file` part (the wording the
/// media upload uses for the same missing parameter).
const MISSING_FILE: &str = "Missing parameter: file";
/// The form field the mascot file is read from (Pleroma's required
/// `:file`).
const FILE_FIELD: &str = "file";
/// The settings-store key the mascot attachment is stored under.
const MASCOT_KEY: &str = "mascot";
/// `id` of the instance default mascot (Pleroma's `User.get_mascot`).
const DEFAULT_MASCOT_ID: &str = "default-mascot";
/// Path appended to `BASE_URL` for the default mascot's URL (Pleroma's
/// `[:assets, :mascots, :pleroma_fox_tan]` url).
const DEFAULT_MASCOT_PATH: &str = "/images/pleroma-fox-tan-smol.png";
/// `pleroma.mime_type` of the default mascot.
const DEFAULT_MASCOT_MIME: &str = "image/png";
/// 422 body for a `frontend_name` the operator did not make pickable, the
/// message the ticket fixes.
const INVALID_FRONTEND: &str = "Invalid frontend";
/// Name of the cookie that mirrors the stored frontend choice.
const FRONTEND_COOKIE: &str = "preferred_frontend";
/// `Max-Age` of the frontend cookie, in seconds: one year, as the ticket
/// fixes it.
const FRONTEND_COOKIE_MAX_AGE: &str = "31536000";

/// The `/api/v1/pleroma` frontend-settings router, merged into
/// [`crate::api::v1`].
///
/// `config` sizes the mascot upload's body limit (see
/// [`upload_body_limit`]); the other routes read it from the state. Do not
/// call [`Router::fallback`] here: unmatched paths must reach the app-wide
/// JSON fallback in [`crate::app`].
pub fn router(config: &Config) -> Router<AppState> {
    Router::new()
        .route(
            "/pleroma/settings/{app}",
            get(get_settings).patch(update_settings),
        )
        .route("/pleroma/mascot", get(get_mascot).put(update_mascot))
        .route(
            "/pleroma/preferred_frontend/available",
            get(available_frontends),
        )
        .route(
            "/pleroma/preferred_frontend",
            put(update_preferred_frontend),
        )
        .layer(upload_body_limit(config))
}

/// The text parameters of `PUT /api/v1/pleroma/mascot`: the endpoint reads
/// only the `file` part, so the text fields (whatever they are) are parsed
/// into a plain [`Value`] and ignored, like the media upload's `thumbnail`.
type MascotFields = Value;

/// The body of `PUT /api/v1/pleroma/preferred_frontend`.
#[derive(Debug, Deserialize)]
struct PreferredFrontendParams {
    /// The frontend the client picked; must be one of
    /// [`Config::available_frontends`].
    frontend_name: String,
}

/// `GET /api/v1/pleroma/settings/{app}` — the account's stored settings
/// for `{app}`, `{}` when it has none.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `read:accounts` check; [`AppError::Unprocessable`] with
/// [`INVALID_APP`] for an unusable `{app}` segment.
async fn get_settings(auth: AuthUser, Path(app): Path<String>) -> AppResult<Json<Value>> {
    auth.require(READ_ACCOUNTS)?;
    check_app_name(&app)?;
    Ok(Json(Value::Object(app_settings(
        &auth.0.settings_store,
        &app,
    ))))
}

/// `PATCH /api/v1/pleroma/settings/{app}` — deep-merge the body into the
/// stored settings, persist the whole store and answer the merged object.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check; [`AppError::Unprocessable`] with
/// [`INVALID_APP`] for an unusable `{app}` segment, with
/// [`STORE_TOO_LARGE`] for a merged store over [`MAX_SETTINGS_STORE_BYTES`]
/// (nothing is written), or with the extractor's wording for a body that
/// is not a JSON object. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn update_settings(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(app): Path<String>,
    ApiBody(patch): ApiBody<Map<String, Value>>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    check_app_name(&app)?;
    let merged = deep_merge(app_settings(&auth.0.settings_store, &app), &patch);
    save_store(
        &state,
        auth.0.id,
        &store_with(&auth.0.settings_store, &app, &merged),
    )
    .await?;
    Ok(Json(Value::Object(merged)))
}

/// `GET /api/v1/pleroma/mascot` — the account's mascot attachment, or the
/// instance default when it stored none (or stored a non-object under the
/// key).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `read:accounts` check.
async fn get_mascot(State(state): State<AppState>, auth: AuthUser) -> AppResult<Json<Value>> {
    auth.require(READ_ACCOUNTS)?;
    let stored = auth
        .0
        .settings_store
        .get(MASCOT_KEY)
        .filter(|value| value.is_object())
        .cloned();
    Ok(Json(
        stored.unwrap_or_else(|| default_mascot(&state.config.base_url)),
    ))
}

/// `PUT /api/v1/pleroma/mascot` — store the uploaded image, record it as
/// the account's mascot and answer the attachment.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check; [`AppError::Unprocessable`] with
/// [`MISSING_FILE`] for a body without a `file` part;
/// [`AppError::UnsupportedMediaType`] with [`MASCOTS_IMAGES_ONLY`] for
/// bytes no image matcher names; [`AppError::PayloadTooLarge`] for a file
/// over `UPLOAD_LIMIT_BYTES`. Storage failures and the settings-store
/// refusals arrive as in [`update_settings`].
async fn update_mascot(
    State(state): State<AppState>,
    auth: AuthUser,
    mut form: MultipartForm<MascotFields>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    let file = form
        .files
        .take(FILE_FIELD)
        .ok_or_else(|| AppError::Unprocessable(MISSING_FILE.to_owned()))?;
    let content_type = image_type(&file.bytes)
        .ok_or_else(|| AppError::UnsupportedMediaType(MASCOTS_IMAGES_ONLY.to_owned()))?;
    let attachment = upload_mascot(&state, auth.0.id, file, content_type).await?;
    save_store(
        &state,
        auth.0.id,
        &store_value_with(&auth.0.settings_store, MASCOT_KEY, &attachment),
    )
    .await?;
    Ok(Json(attachment))
}

/// `GET /api/v1/pleroma/preferred_frontend/available` — the frontends the
/// operator made pickable, in configuration order.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// (no scope: Pleroma's `available_frontends` asks for a token only).
async fn available_frontends(
    State(state): State<AppState>,
    _auth: AuthUser,
) -> AppResult<Json<Vec<String>>> {
    Ok(Json(state.config.available_frontends.clone()))
}

/// `PUT /api/v1/pleroma/preferred_frontend` — store the picked frontend,
/// set the cookie it mirrors and answer `{"frontend_name": name}`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`];
/// [`AppError::Unprocessable`] with [`INVALID_FRONTEND`] for a name the
/// operator did not make pickable (and with the extractor's wording for a
/// body without `frontend_name`); [`AppError::Internal`] if a configured
/// name cannot be written into a header. Any database failure arrives
/// through `?` as [`AppError::Database`].
async fn update_preferred_frontend(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<PreferredFrontendParams>,
) -> AppResult<Response> {
    if !state
        .config
        .available_frontends
        .contains(&params.frontend_name)
    {
        return Err(AppError::Unprocessable(INVALID_FRONTEND.to_owned()));
    }
    user::set_preferred_frontend(&state.db, auth.0.id, &params.frontend_name).await?;
    let cookie = HeaderValue::from_str(&frontend_cookie(&params.frontend_name)).map_err(|err| {
        AppError::Internal(format!(
            "the configured frontend name is not cookie-safe: {err}"
        ))
    })?;
    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "frontend_name": params.frontend_name })),
    )
        .into_response())
}

/// Whether `app` is a usable settings-app name: 1 to [`MAX_APP_NAME_CHARS`]
/// characters of `[A-Za-z0-9_.:-]`.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_APP`] otherwise.
fn check_app_name(app: &str) -> AppResult<()> {
    let usable = !app.is_empty()
        && app.chars().count() <= MAX_APP_NAME_CHARS
        && app.chars().all(is_app_char);
    if usable {
        Ok(())
    } else {
        Err(AppError::Unprocessable(INVALID_APP.to_owned()))
    }
}

/// One character a settings-app name may use: ASCII alphanumerics plus
/// `.`, `_`, `:` and `-`.
const fn is_app_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-')
}

/// The stored settings of `app` as a map: its object when the store holds
/// one under the name, empty otherwise (Pleroma's `get_settings` plus
/// `ensure_object`).
fn app_settings(store: &Value, app: &str) -> Map<String, Value> {
    store.get(app).map_or_else(Map::new, object_of)
}

/// `value` as a map of its object entries; `{}` for anything that is not
/// an object (Pleroma's `ensure_object`).
fn object_of(value: &Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map.clone(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Array(_) => {
            Map::new()
        }
    }
}

/// `patch` deep-merged over `base` (Pleroma's `merge_recursively`): a
/// `null` removes the key, an object merges recursively against the
/// stored value (a stored non-object starts from `{}`), anything else
/// replaces.
fn deep_merge(base: Map<String, Value>, patch: &Map<String, Value>) -> Map<String, Value> {
    let mut merged = base;
    for (key, value) in patch {
        match value {
            Value::Null => {
                let removed: Option<Value> = merged.remove(key);
                drop(removed);
            }
            Value::Object(child) => {
                let stored = merged.remove(key).unwrap_or(Value::Null);
                let nested = deep_merge(object_of(&stored), child);
                let previous: Option<Value> = merged.insert(key.clone(), Value::Object(nested));
                drop(previous);
            }
            replacement @ (Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Array(_)) => {
                let previous: Option<Value> = merged.insert(key.clone(), replacement.clone());
                drop(previous);
            }
        }
    }
    merged
}

/// The store with `app` set to the merged settings of
/// [`update_settings`].
fn store_with(store: &Value, app: &str, settings: &Map<String, Value>) -> Value {
    store_value_with(store, app, &Value::Object(settings.clone()))
}

/// The store with `key` set to `value`, every other entry kept (the root
/// itself is rebuilt, so a non-object store — unreachable through these
/// endpoints, but not through an older row — is replaced rather than
/// corrupted).
fn store_value_with(store: &Value, key: &str, value: &Value) -> Value {
    let mut root = object_of(store);
    let previous: Option<Value> = root.insert(key.to_owned(), value.clone());
    drop(previous);
    Value::Object(root)
}

/// The merged store's size gate: the whole store as JSON text may not
/// exceed [`MAX_SETTINGS_STORE_BYTES`] (the check runs before the write,
/// so a refusal leaves the stored blob untouched).
///
/// Shared with [`update_credentials`](crate::api::accounts), which merges a
/// submitted `pleroma_settings_store` into the same column.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`STORE_TOO_LARGE`] over the limit, or
/// [`AppError::Internal`] for a store that will not serialize (which only
/// a non-finite number could do, and the request parsers never make one).
pub fn check_store_size(store: &Value) -> AppResult<()> {
    let encoded = serde_json::to_vec(store)
        .map_err(|err| AppError::Internal(format!("the settings store did not encode: {err}")))?;
    if encoded.len() > MAX_SETTINGS_STORE_BYTES {
        return Err(AppError::Unprocessable(STORE_TOO_LARGE.to_owned()));
    }
    Ok(())
}

/// `patch` shallow-merged over `base`: Pleroma's `Map.merge`, key by key.
///
/// Every key of `patch` replaces the stored entry under the same name and
/// every other entry of `base` is kept. Unlike this module's `deep_merge`
/// (the one `PATCH /api/v1/pleroma/settings/{app}` runs) this merges top
/// level entries only, and a `null` value is a stored `null` rather than a
/// removal — the whole blob is what `pleroma_settings_store` writes
/// (`User.update_changeset`'s `Map.merge`).
#[must_use]
pub fn shallow_merge(base: &Value, patch: &Map<String, Value>) -> Value {
    let mut merged = object_of(base);
    for (key, value) in patch {
        let previous: Option<Value> = merged.insert(key.clone(), value.clone());
        drop(previous);
    }
    Value::Object(merged)
}

/// The size gate, then the write: [`check_store_size`] decides, and only a
/// store that fits reaches [`set_settings_store`](user::set_settings_store).
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`STORE_TOO_LARGE`] or the
/// serialization failure of [`check_store_size`]; any database failure
/// arrives as [`AppError::Database`].
async fn save_store(state: &AppState, user_id: Uuid, store: &Value) -> AppResult<()> {
    check_store_size(store)?;
    user::set_settings_store(&state.db, user_id, store).await
}

/// The media type of an upload's bytes when [`infer`] names an image for
/// them; `None` for anything else, including bytes no matcher can name.
fn image_type(bytes: &[u8]) -> Option<&'static str> {
    infer::get(bytes)
        .filter(|kind| matches!(kind.matcher_type(), infer::MatcherType::Image))
        .map(|kind| kind.mime_type())
}

/// Store `file` through the uploader and render it as the mascot
/// attachment: a fresh `UUIDv7` id (decision 4), the public URL of the
/// stored key, the fixed `image` type and the detected MIME type in the
/// `pleroma` extension object.
async fn upload_mascot(
    state: &AppState,
    user_id: Uuid,
    file: UploadedFile,
    content_type: &str,
) -> AppResult<Value> {
    let key = uploads::media_key(user_id, file.filename.as_deref(), content_type);
    state.uploader.put(&key, file.bytes, content_type).await?;
    Ok(mascot_attachment(
        &new_id().to_string(),
        &state.uploader.public_url(&key),
        content_type,
    ))
}

/// The attachment JSON a mascot upload answers and stores.
fn mascot_attachment(id: &str, url: &str, mime_type: &str) -> Value {
    json!({
        "id": id,
        "url": url,
        "type": "image",
        "pleroma": { "mime_type": mime_type },
    })
}

/// The instance default mascot: Pleroma's `[:assets, :default_mascot]`
/// tan fox, served from this instance's static path.
fn default_mascot(base_url: &str) -> Value {
    mascot_attachment(
        DEFAULT_MASCOT_ID,
        &format!("{base_url}{DEFAULT_MASCOT_PATH}"),
        DEFAULT_MASCOT_MIME,
    )
}

/// The `Set-Cookie` value mirroring the stored frontend choice: the name
/// as its value, one year of life, valid for the whole origin.
fn frontend_cookie(name: &str) -> String {
    format!("{FRONTEND_COOKIE}={name}; Path=/; Max-Age={FRONTEND_COOKIE_MAX_AGE}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A settings object parsed from its JSON text, for the merge fixtures.
    fn object(text: &str) -> Map<String, Value> {
        let value = serde_json::from_str::<Value>(text).unwrap();
        let Value::Object(map) = value else {
            panic!("the fixture is not a JSON object: {value}");
        };
        map
    }

    /// The merged map rendered back to JSON for comparison.
    fn merged(base: &Value, patch: &Value) -> Value {
        Value::Object(deep_merge(object_of(base), &object_of(patch)))
    }

    #[test]
    fn app_names_are_short_bare_words() {
        for app in [
            "pleroma-fe",
            "pleroma_fe",
            "app.json",
            "fe:v2",
            "A9",
            &"x".repeat(MAX_APP_NAME_CHARS),
        ] {
            assert!(check_app_name(app).is_ok(), "`{app}` is usable");
        }
        for app in [
            "",
            &"x".repeat(MAX_APP_NAME_CHARS + 1),
            "bad name",
            "a/b",
            "ünïcode",
            "fe*v2",
        ] {
            let refused = check_app_name(app).expect_err("the fixture name is refused");
            assert_eq!(
                refused.to_string(),
                INVALID_APP,
                "`{app}` is refused with the 422 message"
            );
            assert!(
                matches!(refused, AppError::Unprocessable(_)),
                "the 422 variant"
            );
        }
    }

    #[test]
    fn app_settings_read_objects_and_only_objects() {
        let store = json!({"pleroma-fe": {"theme": "dark"}, "scalar": 5});
        assert_eq!(
            Value::Object(app_settings(&store, "pleroma-fe")),
            json!({"theme": "dark"}),
            "the stored object comes back"
        );
        assert_eq!(
            Value::Object(app_settings(&store, "missing")),
            json!({}),
            "an absent app is the empty object"
        );
        assert_eq!(
            Value::Object(app_settings(&store, "scalar")),
            json!({}),
            "a non-object stored value reads as empty (Pleroma's ensure_object)"
        );
        assert_eq!(
            Value::Object(app_settings(&json!([]), "pleroma-fe")),
            json!({}),
            "a store that is no object at all reads as empty"
        );
    }

    #[test]
    fn deep_merge_replaces_adds_and_keeps_siblings() {
        assert_eq!(
            merged(&json!({"a": 1, "b": 2}), &json!({"b": 3, "c": 4})),
            json!({"a": 1, "b": 3, "c": 4}),
            "scalars replace, new keys arrive, untouched keys stay"
        );
        assert_eq!(
            merged(&json!({}), &json!({"a": {"b": {"c": 1}}})),
            json!({"a": {"b": {"c": 1}}}),
            "an empty base takes the whole patch tree"
        );
        assert_eq!(
            merged(&json!({"a": 5}), &json!({"a": {"b": 1}})),
            json!({"a": {"b": 1}}),
            "merging an object over a scalar starts from the empty object"
        );
        assert_eq!(
            merged(&json!({"a": {"b": 1}}), &json!({"a": 5})),
            json!({"a": 5}),
            "a scalar over an object replaces the whole subtree"
        );
    }

    #[test]
    fn deep_merge_recurses_and_null_deletes() {
        assert_eq!(
            merged(
                &json!({"a": {"b": {"c": 1, "d": 2}, "e": 3}}),
                &json!({"a": {"b": {"c": null, "f": 9}}})
            ),
            json!({"a": {"b": {"d": 2, "f": 9}, "e": 3}}),
            "the merge reaches the second level: null deletes, siblings stay"
        );
        assert_eq!(
            merged(&json!({"a": 1}), &json!({"a": null})),
            json!({}),
            "null removes the key"
        );
        assert_eq!(
            merged(&json!({"a": {"b": 1}}), &json!({"a": {"b": null}})),
            json!({"a": {}}),
            "a emptied object stays an (empty) object"
        );
        assert_eq!(
            merged(&json!({}), &json!({"ghost": null})),
            json!({}),
            "deleting a key that is not there changes nothing"
        );
    }

    #[test]
    fn shallow_merge_replaces_top_level_keys_only() {
        assert_eq!(
            shallow_merge(
                &json!({"theme": "dark", "fe": {"a": 1, "b": 2}}),
                &object(r#"{"fe": {"c": 3}, "locale": "de"}"#)
            ),
            json!({"theme": "dark", "fe": {"c": 3}, "locale": "de"}),
            "a submitted key wins whole, an untouched one stays, new keys arrive"
        );
        assert_eq!(
            shallow_merge(&json!({"a": 1}), &object(r#"{"a": null}"#)),
            json!({"a": null}),
            "unlike the deep merge, a null is a stored null, not a deletion"
        );
        assert_eq!(
            shallow_merge(&json!({"a": 1}), &Map::new()),
            json!({"a": 1}),
            "an empty patch is the stored blob"
        );
        assert_eq!(
            shallow_merge(&json!("not an object"), &object(r#"{"a": 1}"#)),
            json!({"a": 1}),
            "a stored blob that is no object starts empty (Pleroma's ensure_object)"
        );
    }

    #[test]
    fn store_with_sets_one_key_and_keeps_the_rest() {
        let store = json!({"one": {"x": 1}, "two": 2});
        let after = store_with(&store, "one", &object(r#"{"y": 2}"#));
        assert_eq!(
            after,
            json!({"one": {"y": 2}, "two": 2}),
            "the named entry is replaced whole"
        );
        let added = store_value_with(&store, "three", &json!(true));
        assert_eq!(
            added,
            json!({"one": {"x": 1}, "two": 2, "three": true}),
            "a new key joins the others"
        );
        assert_eq!(
            store_value_with(&json!("junk"), "k", &json!(1)),
            json!({"k": 1}),
            "a non-object store is rebuilt, not corrupted"
        );
    }

    #[test]
    fn the_size_gate_is_the_whole_store_in_bytes() {
        // The JSON text of `{"blob": "…"}` without the padding: the fixed
        // overhead of the wrapper the padded value rides in.
        let overhead = serde_json::to_vec(&json!({"blob": ""})).unwrap().len();
        let exact = json!({"blob": "x".repeat(MAX_SETTINGS_STORE_BYTES - overhead)});
        check_store_size(&exact).expect("exactly 1 MiB fits");
        let over = json!({"blob": "x".repeat(MAX_SETTINGS_STORE_BYTES - overhead + 1)});
        let err = check_store_size(&over).expect_err("one byte over is refused");
        assert_eq!(err.to_string(), STORE_TOO_LARGE, "the refusal message");
    }

    #[test]
    fn a_detected_image_becomes_the_documented_attachment() {
        /// A `PNG`: the signature `infer` matches on. (Detection reads the
        /// magic bytes only, so the fixture is a header, not a file.)
        const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01";
        /// A `PDF`: bytes with a type, just not an image.
        const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF\n";
        /// Text: bytes no matcher can name at all.
        const NOTHING: &[u8] = b"a plain text file, definitely not an image\n";

        assert_eq!(
            image_type(PNG),
            Some("image/png"),
            "a png reads as an image"
        );
        for bytes in [PDF, NOTHING, b""] {
            assert!(image_type(bytes).is_none(), "non-image bytes name no type");
        }

        assert_eq!(
            mascot_attachment("42", "https://cdn.example/media/u/a.png", "image/png"),
            json!({
                "id": "42",
                "url": "https://cdn.example/media/u/a.png",
                "type": "image",
                "pleroma": { "mime_type": "image/png" },
            }),
            "the attachment carries exactly the four documented keys"
        );
    }

    #[test]
    fn the_default_mascot_is_the_pleroma_fox() {
        assert_eq!(
            default_mascot("https://test.example"),
            json!({
                "id": "default-mascot",
                "url": "https://test.example/images/pleroma-fox-tan-smol.png",
                "type": "image",
                "pleroma": { "mime_type": "image/png" },
            }),
            "the ticket's default, hung off the instance origin"
        );
    }

    #[test]
    fn the_frontend_cookie_is_the_documented_one_year_choice() {
        assert_eq!(
            frontend_cookie("pleroma-fe"),
            "preferred_frontend=pleroma-fe; Path=/; Max-Age=31536000",
            "name, path and one-year lifetime, exactly"
        );
    }

    #[test]
    fn an_empty_mascot_form_reads_any_object_of_text_fields() {
        let parsed = serde_json::from_value::<MascotFields>(json!({"thumbnail": "ignored"}))
            .expect("text fields parse into the ignored value");
        assert_eq!(
            parsed,
            json!({"thumbnail": "ignored"}),
            "the fields ride by"
        );
    }
}
