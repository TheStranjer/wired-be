//! The JSON half of `/oauth/authorize` (WBE-666): what the frontend that
//! renders the consent page needs from the API.
//!
//! `GET /oauth/authorize` with `Accept: application/json` runs the
//! request validation and answers `200` with the application's
//! name and website, the scope list, the `redirect_uri` and the client's
//! `state`. (Without that header the endpoint redirects the browser to
//! the frontend instead, see the parent module.)
//!
//! `POST /oauth/authorize` with an `Authorization: Bearer` header (or a
//! JSON body that carries no form-style `authorization` object) is the
//! frontend's verdict on that request: `{client_id, redirect_uri, scope,
//! state, approve}`. Any other `POST` — the retired
//! `authorization[name]`/`authorization[password]` form post included —
//! is refused with a 400 `invalid_request`. The bearer token must belong to a *trusted* app
//! (`oauth_apps.trusted`, set through
//! `PATCH /api/v1/pleroma/admin/oauth_app/{id}`) and cover every
//! requested scope, so a token can never mint a code for more than it
//! holds, nor can an arbitrary app mint codes for another. Approval
//! mints the code for the token's user — no MFA detour, since that user
//! passed it to obtain the token — and answers `{"redirect_uri": …}`
//! carrying `code` and `state` (or `{"code": …}` for the out-of-band
//! URI); a denial answers the redirect with `error=access_denied`.
//!
//! Failures are `{"error", "error_description"}` bodies and are never
//! redirects: 400 `invalid_request`, `invalid_client` or `invalid_scope`
//! for a bad request, 401 for a missing or dead token, 403
//! `unauthorized_client` for an untrusted app and 403 `invalid_scope`
//! for a token that does not cover the request.

use axum::{
    Json,
    http::{
        HeaderMap, StatusCode,
        header::{ACCEPT, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::{
    ACCESS_DENIED, AuthorizeQuery, Delivery, INVALID_CLIENT, INVALID_SCOPE, OOB_REDIRECT_URI,
    check_scopes_subset, load_app, requested_scopes, require_code_response,
    require_registered_redirect, with_query_params,
};
use crate::{
    auth::{self, AuthContext},
    error::{AppError, AppResult},
    extract::into_params,
    models::{
        oauth_app::OauthApp,
        oauth_token::{self, OauthToken},
        user::User,
    },
    oauth::scopes::Scopes,
    state::AppState,
};

/// The one media type a JSON request or `Accept` header names.
const JSON_MEDIA_TYPE: &str = "application/json";
/// `error` of a request that is malformed or names an unsupported flow.
const INVALID_REQUEST_CODE: &str = "invalid_request";
/// `error` of a `client_id` that names no application.
const INVALID_CLIENT_CODE: &str = "invalid_client";
/// `error` of scopes outside the registration or the token.
const INVALID_SCOPE_CODE: &str = "invalid_scope";
/// `error` of a token whose app may not approve for others.
const UNAUTHORIZED_CLIENT_CODE: &str = "unauthorized_client";
/// 401 message when no bearer token was presented.
const NO_TOKEN: &str = "A bearer token is required";
/// 401 message when the bearer token does not resolve to a live user token.
const INVALID_TOKEN: &str = "The access token is invalid";
/// Description of the 403 for an app that is not trusted.
const NOT_TRUSTED: &str = "The token's application is not trusted to approve authorizations";
/// Description of the 403 for a token that lacks a requested scope.
const SCOPE_ESCALATION: &str = "The token does not cover every requested scope";
/// Description of the 400 for a body without the `approve` verdict.
const MISSING_APPROVE: &str = "Missing parameter: approve";
/// Description of the 400 for a `POST` that is not a bearer-token JSON verdict.
const JSON_REQUIRED: &str = "POST /oauth/authorize takes a bearer token and a JSON body";

/// A refusal of the JSON half: an OAuth-shaped body, or any other error
/// answered the way the rest of the API answers it.
#[derive(Debug)]
pub(super) enum JsonFailure {
    /// `{"error": error, "error_description": description}` at `status`.
    Oauth {
        /// The HTTP status.
        status: StatusCode,
        /// The RFC 6749 error code.
        error: &'static str,
        /// What was wrong, for the frontend's log.
        description: String,
    },
    /// An error rendered by [`AppError`] itself (401s, 500s).
    App(AppError),
}

impl JsonFailure {
    /// An OAuth-shaped refusal at `status`.
    fn oauth(status: StatusCode, error: &'static str, description: &str) -> Self {
        Self::Oauth {
            status,
            error,
            description: description.to_owned(),
        }
    }

    /// A 400 `invalid_request` with `description`.
    fn invalid_request(description: &str) -> Self {
        Self::oauth(StatusCode::BAD_REQUEST, INVALID_REQUEST_CODE, description)
    }
}

/// The shared validators answer in [`AppError::BadRequest`] messages;
/// this gives each its OAuth error code. Anything that is not a bad
/// request or a bad parameter shape passes through unchanged.
impl From<AppError> for JsonFailure {
    fn from(err: AppError) -> Self {
        match err {
            AppError::BadRequest(message) | AppError::Unprocessable(message) => {
                let code = match message.as_str() {
                    INVALID_CLIENT => INVALID_CLIENT_CODE,
                    INVALID_SCOPE => INVALID_SCOPE_CODE,
                    _ => INVALID_REQUEST_CODE,
                };
                Self::oauth(StatusCode::BAD_REQUEST, code, &message)
            }
            AppError::Unauthorized(_)
            | AppError::Forbidden(_)
            | AppError::ForbiddenWithIdentifier(_, _)
            | AppError::NotFound
            | AppError::NotFoundMessage(_)
            | AppError::NotImplemented
            | AppError::RouteNotFound
            | AppError::Gone
            | AppError::Conflict(_)
            | AppError::PayloadTooLarge(_)
            | AppError::UnsupportedMediaType(_)
            | AppError::FailedDependency(_)
            | AppError::UpstreamFailure(_)
            | AppError::ServiceUnavailable(_)
            | AppError::RateLimited
            | AppError::BadGateway(_)
            | AppError::Internal(_)
            | AppError::InternalMessage(_)
            | AppError::Database(_) => Self::App(err),
        }
    }
}

impl IntoResponse for JsonFailure {
    fn into_response(self) -> Response {
        match self {
            Self::Oauth {
                status,
                error,
                description,
            } => (
                status,
                Json(json!({ "error": error, "error_description": description })),
            )
                .into_response(),
            Self::App(err) => err.into_response(),
        }
    }
}

/// The request body of the JSON `POST`. Every field is optional at the
/// shape level; the handler answers the missing ones with their own
/// refusals. Unknown fields (`code_challenge` and friends) are ignored.
#[derive(Debug, Default, Deserialize)]
struct ApprovalRequest {
    /// The application being authorized.
    #[serde(default)]
    client_id: Option<String>,
    /// Where the answer goes; must be registered verbatim.
    #[serde(default)]
    redirect_uri: Option<String>,
    /// The requested scopes: a space-separated string or a list.
    #[serde(default)]
    scope: Option<Value>,
    /// The client's opaque `state`, echoed in the answer.
    #[serde(default)]
    state: Option<String>,
    /// The user's verdict.
    #[serde(default)]
    approve: Option<bool>,
}

/// Whether `value` names `application/json`: its media type (parameters
/// dropped) compared case-insensitively, exactly.
fn is_json_media_type(value: &str) -> bool {
    value
        .split(';')
        .next()
        .is_some_and(|media| media.trim().eq_ignore_ascii_case(JSON_MEDIA_TYPE))
}

/// Whether the `Accept` header lists exactly `application/json`: what
/// asks `GET /oauth/authorize` for the app info instead of the redirect
/// to the frontend.
pub(super) fn wants_json(headers: &HeaderMap) -> bool {
    headers
        .get_all(ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(is_json_media_type)
}

/// Whether a `POST` is the frontend's JSON approval rather than the
/// retired consent form: it carries a bearer token, or a JSON body without
/// the form's `authorization` object.
pub(super) fn is_json_approval(headers: &HeaderMap, body: &Value) -> bool {
    if auth::bearer_from_headers(headers).is_some() {
        return true;
    }
    let json_body = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_json_media_type);
    json_body && body.get("authorization").is_none()
}

/// The 400 for a `POST` that is not the frontend's JSON verdict, such as
/// the retired consent form post.
pub(super) fn refuse_form() -> Response {
    JsonFailure::invalid_request(JSON_REQUIRED).into_response()
}

/// `GET /oauth/authorize` as JSON: the validated request's description,
/// or the refusal.
pub(super) async fn describe(state: &AppState, query: &AuthorizeQuery) -> Response {
    match describe_request(state, query).await {
        Ok(body) => Json(body).into_response(),
        Err(failure) => failure.into_response(),
    }
}

/// The validation of the `GET`, in the order the redirect applies it, and the body.
async fn describe_request(state: &AppState, query: &AuthorizeQuery) -> Result<Value, JsonFailure> {
    require_code_response(query.response_type.as_deref())?;
    let redirect_uri = query.redirect_uri.as_deref().unwrap_or_default();
    let (app, scopes) = validated(
        &state.db,
        query.client_id.as_deref(),
        redirect_uri,
        query.scope.as_deref(),
    )
    .await?;
    Ok(json!({
        "app": { "name": app.client_name, "website": app.website },
        "scopes": scopes.names(),
        "redirect_uri": redirect_uri,
        "state": query.state,
    }))
}

/// The application behind `client_id`, its registered `redirect_uri` and
/// the requested scopes inside its registration — the checks both halves
/// share.
async fn validated(
    pool: &PgPool,
    client_id: Option<&str>,
    redirect_uri: &str,
    scope: Option<&str>,
) -> AppResult<(OauthApp, Scopes)> {
    let app = load_app(pool, client_id).await?;
    require_registered_redirect(&app, redirect_uri)?;
    let scopes = requested_scopes(scope);
    check_scopes_subset(&scopes, &app)?;
    Ok((app, scopes))
}

/// `POST /oauth/authorize` as JSON: the frontend's verdict applied, or
/// the refusal.
pub(super) async fn approve(state: &AppState, headers: &HeaderMap, body: Value) -> Response {
    match apply_verdict(state, headers, body).await {
        Ok(answer) => Json(answer).into_response(),
        Err(failure) => failure.into_response(),
    }
}

/// The checks in order — token, trusted app, body, request, scope cover,
/// verdict — and the answer body.
async fn apply_verdict(
    state: &AppState,
    headers: &HeaderMap,
    body: Value,
) -> Result<Value, JsonFailure> {
    let (user, token, token_app) = authenticate(state, headers).await?;
    if !token_app.trusted {
        return Err(JsonFailure::oauth(
            StatusCode::FORBIDDEN,
            UNAUTHORIZED_CLIENT_CODE,
            NOT_TRUSTED,
        ));
    }
    let request: ApprovalRequest = into_params(body)?;
    let redirect_uri = request.redirect_uri.as_deref().unwrap_or_default();
    let scope = scope_text(request.scope.as_ref());
    let (app, scopes) = validated(
        &state.db,
        request.client_id.as_deref(),
        redirect_uri,
        scope.as_deref(),
    )
    .await?;
    if !scopes.is_subset_of(&Scopes::parse(&token.scopes)) {
        return Err(JsonFailure::oauth(
            StatusCode::FORBIDDEN,
            INVALID_SCOPE_CODE,
            SCOPE_ESCALATION,
        ));
    }
    let Some(approved) = request.approve else {
        return Err(JsonFailure::invalid_request(MISSING_APPROVE));
    };
    let delivery = Delivery {
        redirect_uri,
        state: request.state.as_deref(),
    };
    if !approved {
        return Ok(json!({ "redirect_uri": redirect_with(&delivery, ("error", ACCESS_DENIED)) }));
    }
    let authorization = oauth_token::create_authorization(
        &state.db,
        user.id,
        app.id,
        &scopes.to_string(),
        redirect_uri,
    )
    .await?;
    if redirect_uri == OOB_REDIRECT_URI {
        return Ok(json!({ "code": authorization.token }));
    }
    Ok(json!({ "redirect_uri": redirect_with(&delivery, ("code", authorization.token.as_str())) }))
}

/// The user, token and app behind the request's bearer token.
///
/// # Errors
///
/// 401 when no token is presented, or it is unknown, expired, app-only or
/// its owner is not active; database failures pass through.
async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(User, OauthToken, OauthApp), JsonFailure> {
    let Some(raw) = auth::bearer_from_headers(headers) else {
        return Err(AppError::Unauthorized(NO_TOKEN.to_owned()).into());
    };
    let context = auth::credential(state, &raw).await?;
    match context {
        Some(AuthContext {
            token,
            user: Some(user),
            app,
        }) => Ok((user, token, app)),
        Some(AuthContext { user: None, .. }) | None => {
            Err(AppError::Unauthorized(INVALID_TOKEN.to_owned()).into())
        }
    }
}

/// The scope text of a body's `scope`: a string as sent, a list joined
/// by spaces; anything else asks for nothing (the default scope).
fn scope_text(scope: Option<&Value>) -> Option<String> {
    match scope {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<&str>>()
                .join(" "),
        ),
        Some(Value::Bool(_) | Value::Null | Value::Number(_) | Value::Object(_)) | None => None,
    }
}

/// The delivery's `redirect_uri` with `first` and the client's `state`
/// (when it sent one) merged into its query.
fn redirect_with(delivery: &Delivery<'_>, first: (&str, &str)) -> String {
    let mut params: Vec<(&str, &str)> = vec![first];
    if let Some(client_state) = delivery.state {
        params.push(("state", client_state));
    }
    with_query_params(delivery.redirect_uri, &params)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::super::{INVALID_REDIRECT_URI, UNSUPPORTED_RESPONSE_TYPE};
    use super::*;

    /// Headers holding one `name: value` pair.
    fn headers_with(name: axum::http::HeaderName, value: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let _absent = headers.insert(name, HeaderValue::from_static(value));
        headers
    }

    #[test]
    fn accept_must_name_application_json_exactly() {
        let json_accept = headers_with(ACCEPT, "application/json");
        assert!(wants_json(&json_accept), "the plain media type");
        let listed = headers_with(ACCEPT, "text/plain, Application/JSON; q=0.9");
        assert!(wants_json(&listed), "one of a list, any case, with params");
        for other in [
            "text/*,application/xhtml+xml,*/*;q=0.8",
            "application/jsonp",
            "application/activity+json",
            "*/*",
        ] {
            assert!(
                !wants_json(&headers_with(ACCEPT, other)),
                "`{other}` is not application/json"
            );
        }
        assert!(!wants_json(&HeaderMap::new()), "no header, no JSON");
    }

    #[test]
    fn approvals_are_told_from_the_retired_consent_form() {
        let bearer = headers_with(axum::http::header::AUTHORIZATION, "Bearer abc");
        assert!(is_json_approval(&bearer, &json!({})), "a bearer token");
        let json_type = headers_with(CONTENT_TYPE, "application/json; charset=utf-8");
        assert!(
            is_json_approval(&json_type, &json!({ "approve": true })),
            "a JSON body"
        );
        assert!(
            !is_json_approval(&json_type, &json!({ "authorization": {} })),
            "a JSON body in the form's own shape stays the form"
        );
        let form_type = headers_with(CONTENT_TYPE, "application/x-www-form-urlencoded");
        assert!(
            !is_json_approval(&form_type, &json!({ "authorization": {} })),
            "a form post"
        );
        assert!(!is_json_approval(&HeaderMap::new(), &json!({})), "nothing");
    }

    #[test]
    fn scope_text_reads_strings_and_lists() {
        assert_eq!(
            scope_text(Some(&json!("read write"))).as_deref(),
            Some("read write"),
            "a string as sent"
        );
        assert_eq!(
            scope_text(Some(&json!(["read", "write", 7]))).as_deref(),
            Some("read write"),
            "a list joined, non-strings dropped"
        );
        assert_eq!(scope_text(Some(&json!(7))), None, "a number asks nothing");
        assert_eq!(scope_text(None), None, "absent asks nothing");
    }

    #[test]
    fn validator_messages_map_to_oauth_codes() {
        for (message, code) in [
            (INVALID_CLIENT, INVALID_CLIENT_CODE),
            (INVALID_SCOPE, INVALID_SCOPE_CODE),
            (INVALID_REDIRECT_URI, INVALID_REQUEST_CODE),
            (UNSUPPORTED_RESPONSE_TYPE, INVALID_REQUEST_CODE),
            ("anything else", INVALID_REQUEST_CODE),
        ] {
            let failure = JsonFailure::from(AppError::BadRequest(message.to_owned()));
            assert!(
                matches!(&failure, JsonFailure::Oauth { status, error, .. }
                    if *status == StatusCode::BAD_REQUEST && *error == code),
                "`{message}` maps to {code}: {failure:?}"
            );
        }
        let passed = JsonFailure::from(AppError::Unauthorized("x".to_owned()));
        assert!(matches!(passed, JsonFailure::App(_)), "401 passes through");
    }

    #[test]
    fn redirects_carry_the_state_when_there_is_one() {
        let with_state = Delivery {
            redirect_uri: "https://c.example/cb",
            state: Some("a b"),
        };
        assert_eq!(
            redirect_with(&with_state, ("code", "xyz")),
            "https://c.example/cb?code=xyz&state=a+b",
            "code then state"
        );
        let without = Delivery {
            redirect_uri: "https://c.example/cb",
            state: None,
        };
        assert_eq!(
            redirect_with(&without, ("error", ACCESS_DENIED)),
            "https://c.example/cb?error=access_denied",
            "no state, none echoed"
        );
    }
}
