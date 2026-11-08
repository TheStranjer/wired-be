//! OAuth token management over HTTP: the caller's active authorizations
//! and the applications behind them (`docs/ARCHITECTURE.md` decision 1).
//!
//! The references are Pleroma's `TokenController`
//! (`lib/pleroma/web/o_auth/token_controller.ex`, routed in
//! `lib/pleroma/web/router.ex`) and the `PleromaAPI` `AppController`
//! (`lib/pleroma/web/pleroma_api/controllers/app_controller.ex`,
//! `GET /api/v1/pleroma/apps`). The ticket's shapes are authoritative:
//!
//! - `GET /api/oauth_tokens` — the caller's **valid, unexpired** tokens
//!   as `[{"id", "app_name", "valid_until"}]`, newest first. Unlike
//!   Pleroma's `TokenView`, no `scopes` key; unlike its unfiltered
//!   `Token.get_user_tokens`, expiry is applied here. No particular
//!   scope is required beyond a user token.
//! - `DELETE /api/oauth_tokens/{id}` — revoke one of the caller's own
//!   tokens and answer `200 {}`. Pleroma always answers 201 with an
//!   empty body; here a foreign or unknown (or malformed) id is the
//!   404 `{"error":"Record not found"}` of decision 8 instead.
//! - `GET /api/v1/pleroma/apps` — the applications the caller knows:
//!   those they registered (`oauth_apps.user_id`) **or** hold a token
//!   for, deduplicated, newest app first, rendered with the eight keys
//!   `id`, `name`, `website`, `redirect_uri`, `client_id`,
//!   `client_secret`, `trusted` and `vapid_key` (Pleroma's
//!   `MastodonAPI.AppView.show.json` plus the flag its admin view adds;
//!   `vapid_key` is the instance VAPID public key, which Pleroma's
//!   `with_vapid_key` puts on the render once push is configured — it
//!   always is here, see [`crate::services::vapid`]).
//!
//! All three routes require a user token ([`AuthUser`]); none demands a
//! particular scope. The queries live in [`crate::models::oauth_token`]
//! and [`crate::models::oauth_app`].

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{delete, get},
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value};
use tracing::debug;

use crate::{
    auth::AuthUser,
    error::{AppError, AppResult},
    models::{oauth_app, oauth_token},
    services::vapid,
    state::AppState,
    util::id::parse_id,
};

/// The `/api/oauth_tokens` router, nested at that path in [`crate::api`].
///
/// Do not call [`Router::fallback`] here: the app-wide JSON fallback in
/// [`crate::app`] must answer the unmatched paths.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_tokens))
        .route("/{id}", delete(revoke_token))
}

/// The `GET /api/v1/pleroma/apps` router, nested at `/pleroma/apps` in
/// [`crate::api::v1`].
pub fn apps_router() -> Router<AppState> {
    Router::new().route("/", get(list_apps))
}

/// `GET /api/oauth_tokens` — the caller's valid tokens, newest first.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`].
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn list_tokens(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<Vec<TokenEntry>>> {
    let rows = oauth_token::list_valid_with_app_names(&state.db, auth.0.id).await?;
    debug!(
        count = rows.len(),
        user = %auth.0.id,
        "oauth tokens listed"
    );
    Ok(Json(rows.iter().map(TokenEntry::from_row).collect()))
}

/// `DELETE /api/oauth_tokens/{id}` — revoke one of the caller's own
/// tokens and answer `200 {}`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`];
/// [`AppError::NotFound`] when the id is malformed, names no token or
/// names another user's (Pleroma's silent no-op reads as a 404 here).
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn revoke_token(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let Some(token_id) = parse_id(&id) else {
        return Err(AppError::NotFound);
    };
    if !oauth_token::delete_for_user(&state.db, auth.0.id, token_id).await? {
        return Err(AppError::NotFound);
    }
    debug!(
        token = %token_id,
        user = %auth.0.id,
        "oauth token revoked"
    );
    Ok(Json(Value::Object(Map::new())))
}

/// `GET /api/v1/pleroma/apps` — the apps the caller registered or holds
/// a token for, deduplicated, newest first.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`].
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn list_apps(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<Vec<PleromaApp>>> {
    let apps = oauth_app::list_for_user(&state.db, auth.0.id).await?;
    let vapid_key = vapid::vapid_public_key(&state.db).await?;
    debug!(
        count = apps.len(),
        user = %auth.0.id,
        "pleroma apps listed"
    );
    Ok(Json(
        apps.iter()
            .map(|app| PleromaApp::from_row(app, &vapid_key))
            .collect(),
    ))
}

/// One entry of `GET /api/oauth_tokens`: exactly the three keys
/// `id`, `app_name` and `valid_until` the ticket pins (Pleroma's
/// `TokenView.show.json` without its `scopes`).
#[derive(Debug, Serialize)]
struct TokenEntry {
    /// The token's `UUIDv7` id as lowercase hyphenated text (decision 4).
    id: String,
    /// The name of the application that issued the token.
    app_name: String,
    /// When the token expires, in the Mastodon wire format (decision 5).
    #[serde(with = "crate::util::time::serde_mastodon_time")]
    valid_until: DateTime<Utc>,
}

impl TokenEntry {
    /// Renders one join row from [`oauth_token::list_valid_with_app_names`].
    fn from_row(row: &oauth_token::TokenWithAppName) -> Self {
        Self {
            id: row.id.to_string(),
            app_name: row.app_name.clone(),
            valid_until: row.valid_until,
        }
    }
}

/// One entry of `GET /api/v1/pleroma/apps`: the keys Pleroma's
/// `MastodonAPI.AppView.show.json` renders (extended with `trusted` and
/// the configured `vapid_key`), built from a stored
/// [`oauth_app::OauthApp`].
#[derive(Debug, Serialize)]
struct PleromaApp {
    /// The app's `UUIDv7` id as lowercase hyphenated text (decision 4).
    id: String,
    /// The registered application name.
    name: String,
    /// The application homepage, `null` when unset.
    website: Option<String>,
    /// The redirect URIs, exactly as stored.
    redirect_uri: String,
    /// The public OAuth client id.
    client_id: String,
    /// The OAuth client secret (Pleroma hands it to the app's owner).
    client_secret: String,
    /// Whether the app skips the authorization confirmation screen.
    trusted: bool,
    /// The instance VAPID public key Pleroma's `with_vapid_key` adds.
    vapid_key: String,
}

impl PleromaApp {
    /// Renders one stored app row with the instance `vapid_key`.
    fn from_row(app: &oauth_app::OauthApp, vapid_key: &str) -> Self {
        Self {
            id: app.id.to_string(),
            name: app.client_name.clone(),
            website: app.website.clone(),
            redirect_uri: app.redirect_uris.clone(),
            client_id: app.client_id.clone(),
            client_secret: app.client_secret.clone(),
            trusted: app.trusted,
            vapid_key: vapid_key.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::util::{id::new_id, time::parse_time};

    /// A join row with a known id, app name and expiry.
    fn token_row() -> oauth_token::TokenWithAppName {
        oauth_token::TokenWithAppName {
            id: new_id(),
            app_name: "Mobile Client".to_owned(),
            valid_until: parse_time("2024-01-31T12:34:56.789Z").unwrap(),
        }
    }

    /// A stored app row; everything [`new_app`](crate::models::oauth_app)
    /// does not mint is set to known text.
    fn app_row() -> oauth_app::OauthApp {
        oauth_app::OauthApp {
            id: new_id(),
            client_name: "Entity".to_owned(),
            redirect_uris: "https://a.example/cb\nhttps://b.example/cb".to_owned(),
            scopes: "read write".to_owned(),
            website: None,
            client_id: "cid".to_owned(),
            client_secret: "csec".to_owned(),
            trusted: true,
            user_id: None,
            inserted_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn a_token_entry_carries_exactly_the_three_pinned_keys() {
        let row = token_row();
        let id = row.id.to_string();
        let json = serde_json::to_value(TokenEntry::from_row(&row)).unwrap();
        assert_eq!(
            json,
            json!({
                "id": id,
                "app_name": "Mobile Client",
                "valid_until": "2024-01-31T12:34:56.789Z",
            }),
            "the uuid as hyphenated text, the joined name, the Mastodon time"
        );
        assert_eq!(json.as_object().map(Map::len), Some(3), "no other keys");
    }

    #[test]
    fn a_pleroma_app_carries_exactly_the_eight_pinned_keys() {
        let app = app_row();
        let id = app.id.to_string();
        let json = serde_json::to_value(PleromaApp::from_row(&app, "vapid-pub")).unwrap();
        assert_eq!(
            json,
            json!({
                "id": id,
                "name": "Entity",
                "website": null,
                "redirect_uri": "https://a.example/cb\nhttps://b.example/cb",
                "client_id": "cid",
                "client_secret": "csec",
                "trusted": true,
                "vapid_key": "vapid-pub",
            }),
            "client_name becomes name, redirect_uris becomes redirect_uri"
        );
        assert_eq!(json.as_object().map(Map::len), Some(8), "no other keys");
    }
}
