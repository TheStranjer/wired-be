//! `POST /api/v1/apps` and `GET /api/v1/apps/verify_credentials`
//! (Pleroma's `AppController`,
//! `lib/pleroma/web/mastodon_api/controllers/app_controller.ex`).
//!
//! Registering is how a client obtains the `client_id`/`client_secret`
//! it later exchanges at `/oauth`: the endpoint takes no authentication.
//! The parameters are `client_name` (required), `redirect_uris`
//! (required, a string or a list), `scopes` (default `read`, validated
//! per `docs/ARCHITECTURE.md` decision 9) and `website` (optional), all
//! read through the [`ApiBody`] extractor (decision 10). Missing or
//! empty required fields, and unknown scopes, are 422s (decision 8).
//!
//! `GET /api/v1/apps/verify_credentials` is the same [`Application`]
//! entity without its secrets, so a client can confirm the token it was
//! issued and learn which application it belongs to. It authenticates
//! with [`AppAuth`]: any valid bearer token — user-bound or app-only —
//! and no scope is required (Pleroma's `verify_credentials` accepts
//! both, `AppController.verify_credentials`). A missing, unknown or
//! expired token is the 401 "The access token is invalid".

use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use serde::Deserialize;

use crate::{
    auth::AppAuth,
    entities::application::Application,
    error::{AppError, AppResult},
    extract::ApiBody,
    models::oauth_app::{self, NewOauthApp},
    oauth::scopes::Scopes,
    state::AppState,
};

/// The `/api/v1/apps` router, merged into [`crate::api`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/apps", post(create_app))
        .route("/apps/verify_credentials", get(verify_credentials))
}

/// `POST /api/v1/apps` — register an application and hand back its OAuth
/// credentials. No authentication: this is where clients get their
/// credentials from.
///
/// # Errors
///
/// [`AppError::Unprocessable`] when `client_name` or `redirect_uris` is
/// missing or empty, or a requested scope is unknown. Any database
/// failure arrives through `?` as [`AppError::Database`].
async fn create_app(
    State(state): State<AppState>,
    ApiBody(params): ApiBody<CreateAppParams>,
) -> AppResult<Json<Application>> {
    let scopes = resolve_scopes(params.scopes.as_deref())?;
    let redirect_uris = normalize_redirect_uris(params.redirect_uris);
    if redirect_uris.trim().is_empty() {
        return Err(AppError::Unprocessable(
            "redirect_uris must not be empty".to_owned(),
        ));
    }
    let app = oauth_app::create(
        &state.db,
        &NewOauthApp {
            client_name: params.client_name,
            redirect_uris,
            scopes: scopes.to_string(),
            website: params.website,
            user_id: None,
        },
    )
    .await?;
    Ok(Json(Application::from_app(app, true)))
}

/// `GET /api/v1/apps/verify_credentials` — the application behind the
/// presented bearer token, without its secrets.
///
/// [`AppAuth`] has already resolved the token to its issuing
/// application (rejecting missing, unknown, expired or deactivated-owner
/// tokens with a 401), so there is nothing left to fail here.
async fn verify_credentials(AppAuth(app, _token): AppAuth) -> Json<Application> {
    Json(Application::from_app(app, false))
}

/// The request parameters of `POST /api/v1/apps`.
#[derive(Debug, Deserialize)]
struct CreateAppParams {
    /// Human-facing application name; required.
    client_name: String,
    /// Redirect URI(s), one string or a list; required and non-empty.
    redirect_uris: RedirectUris,
    /// Scope string; absent or blank means [`Scopes::default`].
    #[serde(default)]
    scopes: Option<String>,
    /// Optional application homepage.
    #[serde(default)]
    website: Option<String>,
}

/// `redirect_uris` as clients send it: one string — which may itself
/// hold several space- or newline-separated URIs — or a list (a JSON
/// array, or form `redirect_uris[]=…` keys).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RedirectUris {
    /// A single string, passed through to storage verbatim.
    Single(String),
    /// A list of URIs, joined into the stored newline-separated form.
    Multiple(Vec<String>),
}

/// The stored spelling of `redirect_uris`: a list is trimmed, has its
/// blank entries dropped and is joined with newlines (Pleroma's
/// `normalize_redirect_uris`, `lib/pleroma/web/o_auth/app.ex`); a lone
/// string is kept as the client sent it. The empty or blank result this
/// can leave is what the endpoint rejects with 422.
fn normalize_redirect_uris(input: RedirectUris) -> String {
    match input {
        RedirectUris::Single(text) => text,
        RedirectUris::Multiple(uris) => uris
            .iter()
            .map(String::as_str)
            .map(str::trim)
            .filter(|uri| !uri.is_empty())
            .collect::<Vec<&str>>()
            .join("\n"),
    }
}

/// The scopes to register: parsed from the request with
/// [`Scopes::parse`], defaulting to [`Scopes::default`] (`read`) when
/// absent or blank.
///
/// # Errors
///
/// [`AppError::Unprocessable`] naming the first scope
/// [`Scopes::validate`] rejects.
fn resolve_scopes(text: Option<&str>) -> AppResult<Scopes> {
    let scopes = text
        .map(str::trim)
        .filter(|trimmed| !trimmed.is_empty())
        .map_or_else(Scopes::default, Scopes::parse);
    scopes
        .validate()
        .map_err(|reason| AppError::Unprocessable(format!("Invalid scopes: {reason}")))?;
    Ok(scopes)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::extract::into_params;

    /// Parses `value` as the registration endpoint's parameters, as
    /// `ApiBody` would.
    fn params_of(value: serde_json::Value) -> AppResult<CreateAppParams> {
        into_params(value)
    }

    #[test]
    fn a_list_of_redirect_uris_is_trimmed_joined_and_stored() {
        let uris = RedirectUris::Multiple(vec![
            " https://a.example/cb ".to_owned(),
            String::new(),
            "https://b.example/cb".to_owned(),
        ]);
        assert_eq!(
            normalize_redirect_uris(uris),
            "https://a.example/cb\nhttps://b.example/cb",
            "blanks drop, the rest joins with newlines"
        );
    }

    #[test]
    fn a_single_redirect_uri_string_is_kept_verbatim() {
        let text = "https://a.example/cb https://b.example/cb";
        assert_eq!(
            normalize_redirect_uris(RedirectUris::Single(text.to_owned())),
            text,
            "space-separated input reaches storage as sent"
        );
    }

    #[test]
    fn an_all_blank_list_normalizes_to_empty() {
        let uris = RedirectUris::Multiple(vec!["  ".to_owned(), String::new()]);
        assert_eq!(
            normalize_redirect_uris(uris),
            "",
            "nothing left to store, which the endpoint rejects"
        );
    }

    #[test]
    fn scopes_default_to_read_when_absent_or_blank() {
        for absent in [None, Some(""), Some("   ")] {
            assert_eq!(
                resolve_scopes(absent).unwrap().to_string(),
                Scopes::default().to_string(),
                "`{absent:?}` falls back to the default scope"
            );
        }
    }

    #[test]
    fn requested_scopes_parse_and_validate() {
        let scopes = resolve_scopes(Some("read+write follow")).unwrap();
        assert_eq!(scopes.to_string(), "read write follow", "the list");
        assert_eq!(
            resolve_scopes(Some("read push")).unwrap().to_string(),
            "read push",
            "space-separated too"
        );
    }

    #[test]
    fn an_unknown_scope_is_unprocessable() {
        let err = resolve_scopes(Some("read bogus")).expect_err("bogus must fail");
        assert!(
            matches!(err, AppError::Unprocessable(_)),
            "a bad scope is a 422, got {err}"
        );
        assert!(
            err.to_string().contains("bogus"),
            "the message names the offender"
        );
    }

    #[test]
    fn json_bodies_deserialize_to_the_registration_params() {
        let full: CreateAppParams = params_of(json!({
            "client_name": "C",
            "redirect_uris": ["a", "b"],
            "scopes": "read",
            "website": "https://c.example",
        }))
        .unwrap();
        assert_eq!(full.client_name, "C", "the name");
        assert!(
            matches!(full.redirect_uris, RedirectUris::Multiple(_)),
            "a json array is the list form"
        );
        assert_eq!(full.scopes.as_deref(), Some("read"), "a scopes field");
        assert_eq!(
            full.website.as_deref(),
            Some("https://c.example"),
            "a website field"
        );

        let minimal: CreateAppParams =
            params_of(json!({"client_name": "C", "redirect_uris": "a b"})).unwrap();
        assert!(
            matches!(minimal.redirect_uris, RedirectUris::Single(_)),
            "a json string is the single form"
        );
        assert!(
            minimal.scopes.is_none() && minimal.website.is_none(),
            "both optional"
        );
    }

    #[test]
    fn missing_required_fields_are_unprocessable() {
        let name_err =
            params_of(json!({"redirect_uris": "x"})).expect_err("client_name is required");
        assert!(
            matches!(name_err, AppError::Unprocessable(_)),
            "a missing client_name is a 422"
        );
        let uris_err =
            params_of(json!({"client_name": "x"})).expect_err("redirect_uris is required");
        assert!(
            matches!(uris_err, AppError::Unprocessable(_)),
            "missing redirect_uris is a 422"
        );
    }
}
