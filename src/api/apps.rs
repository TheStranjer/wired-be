//! `POST /api/v1/apps`: client application registration
//! (Pleroma's `AppController.create`,
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
//! The response is the Mastodon `Application` entity with credentials —
//! Pleroma's `show.json` (`lib/pleroma/web/mastodon_api/views/app_view.ex`)
//! extended with the Mastodon fields. `vapid_key` is emitted as `null`
//! here; the web-push ticket fills it in.

use axum::{Json, Router, extract::State, routing::post};
use serde::{Deserialize, Serialize};

use crate::{
    error::{AppError, AppResult},
    extract::ApiBody,
    models::oauth_app::{self, NewOauthApp},
    oauth::scopes::Scopes,
    state::AppState,
};

/// `client_secret_expires_at` of the response: these secrets never
/// expire, which Mastodon spells `0`.
const NEVER_EXPIRES: i64 = 0;

/// The `/api/v1/apps` router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new().route("/apps", post(create_app))
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
) -> AppResult<Json<ApplicationWithCredentials>> {
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
    Ok(Json(ApplicationWithCredentials::from(app)))
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

/// The Mastodon `Application` entity with credentials, the 200 body of a
/// successful registration. `vapid_key` stays `null` until the push
/// ticket owns it.
#[derive(Debug, Serialize)]
struct ApplicationWithCredentials {
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
    /// The OAuth client secret.
    client_secret: String,
    /// `0`: client secrets do not expire.
    client_secret_expires_at: i64,
    /// Web-push public key; `null` until the push ticket fills it in.
    vapid_key: Option<String>,
    /// The registered scopes as a list.
    scopes: Vec<String>,
}

/// The stored scope string as the JSON list the API returns.
fn scope_list(stored: &str) -> Vec<String> {
    stored
        .split(' ')
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Renders a freshly created row as the API response.
impl From<oauth_app::OauthApp> for ApplicationWithCredentials {
    fn from(app: oauth_app::OauthApp) -> Self {
        Self {
            id: app.id.to_string(),
            name: app.client_name,
            website: app.website,
            redirect_uri: app.redirect_uris,
            client_id: app.client_id,
            scopes: scope_list(&app.scopes),
            vapid_key: None,
            client_secret: app.client_secret,
            client_secret_expires_at: NEVER_EXPIRES,
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        extract::FromRequest as _,
        http::{Request, header::CONTENT_TYPE},
    };
    use serde_json::json;

    use super::*;
    use crate::extract::into_params;

    /// Media types the extractor parses, spelled as clients send them.
    const JSON_MIME: &str = "application/json";
    /// The urlencoded media type.
    const FORM_MIME: &str = "application/x-www-form-urlencoded";

    /// Parses `value` as the endpoint's parameters, as `ApiBody` would.
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
    fn json_and_form_bodies_deserialize_to_the_same_params() {
        // The extractor turns both content types into this shape
        // (`src/extract.rs` covers the parsing itself).
        let json: CreateAppParams =
            params_of(json!({"client_name": "C", "redirect_uris": ["a", "b"]})).unwrap();
        assert!(
            matches!(json.redirect_uris, RedirectUris::Multiple(_)),
            "a json array is the list form"
        );
        assert!(
            json.scopes.is_none() && json.website.is_none(),
            "both optional"
        );

        let single: CreateAppParams =
            params_of(json!({"client_name": "C", "redirect_uris": "a b"})).unwrap();
        assert!(
            matches!(single.redirect_uris, RedirectUris::Single(_)),
            "a json string is the single form"
        );

        let form: CreateAppParams = params_of(json!({
            "client_name": "C",
            "redirect_uris": ["a", "b"],
            "scopes": "read",
        }))
        .unwrap();
        assert_eq!(form.scopes.as_deref(), Some("read"), "a form scopes field");
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

    #[tokio::test]
    async fn an_empty_body_is_an_unprocessable_request() {
        // The `ApiBody` extractor sees `{}`; the required fields reject.
        let request = Request::post("/api/v1/apps")
            .header(CONTENT_TYPE, JSON_MIME)
            .body(axum::body::Body::empty())
            .unwrap();
        let err = ApiBody::<CreateAppParams>::from_request(request, &())
            .await
            .expect_err("no parameters at all");
        assert!(
            matches!(err, AppError::Unprocessable(_)),
            "an empty body is a 422, got {err}"
        );
    }

    #[tokio::test]
    async fn a_form_body_reaches_the_same_params() {
        let request = Request::post("/api/v1/apps")
            .header(CONTENT_TYPE, FORM_MIME)
            .body(axum::body::Body::from(
                "client_name=Form+Client&redirect_uris%5B%5D=a&redirect_uris%5B%5D=b",
            ))
            .unwrap();
        let ApiBody(params) = ApiBody::<CreateAppParams>::from_request(request, &())
            .await
            .unwrap();
        assert_eq!(params.client_name, "Form Client", "the form name decodes");
        assert_eq!(
            normalize_redirect_uris(params.redirect_uris),
            "a\nb",
            "list form keys join"
        );
    }

    #[test]
    fn the_response_renders_the_entity() {
        let app = oauth_app::OauthApp {
            id: crate::util::id::new_id(),
            client_name: "Entity".to_owned(),
            redirect_uris: "https://a.example/cb\nhttps://b.example/cb".to_owned(),
            scopes: "read write".to_owned(),
            website: None,
            client_id: "cid".to_owned(),
            client_secret: "csec".to_owned(),
            trusted: false,
            user_id: None,
            inserted_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let id = app.id.to_string();
        let json = serde_json::to_value(ApplicationWithCredentials::from(app)).unwrap();
        assert_eq!(json["id"], json!(id), "the uuid as hyphenated text");
        assert_eq!(json["name"], "Entity", "client_name becomes name");
        assert!(json["website"].is_null(), "an unset website is null");
        assert_eq!(
            json["redirect_uri"], "https://a.example/cb\nhttps://b.example/cb",
            "redirect_uri is the stored string"
        );
        assert_eq!(json["client_id"], "cid", "the client id is handed back");
        assert_eq!(
            json["client_secret"], "csec",
            "the client secret is handed back"
        );
        assert_eq!(json["client_secret_expires_at"], 0, "secrets do not expire");
        assert!(json["vapid_key"].is_null(), "vapid_key is null for now");
        assert_eq!(json["scopes"], json!(["read", "write"]), "the scope list");
    }
}
