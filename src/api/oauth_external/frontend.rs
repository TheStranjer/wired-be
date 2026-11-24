//! The JSON half of an external login, for a frontend that renders the
//! registration and consent screens itself: `GET /oauth/registration_details`
//! with `Accept: application/json` ([`details_json`]), `POST /oauth/register`
//! with a JSON body ([`registered_json`]), and the consent pair
//! `GET`/`POST /oauth/external/consent` ([`consent_details`],
//! [`consent_decided`]) a linked account of an untrusted application is sent
//! to.
//!
//! The registration answers are the HTML page's outcomes spelled as JSON: a
//! finished login is 200 `{"redirect_uri": "<app redirect>?code=…&state=…"}`
//! (`{"code": …}` for the out-of-band URI), an account waiting for staff or
//! for its email is 200 `{"pending": "approval"|"confirmation"}`, a refused
//! submission is `POST /api/v1/accounts`'s 422 (`error` carrying the JSON
//! field map) and a closed instance its 403. An MFA-enabled account's
//! `redirect_uri` is the `/oauth/mfa` detour, which the frontend follows.
//!
//! The consent state carries a `confirmation` document — the one-time
//! `confirm_token` and the linked user's id — that the callback stores
//! ([`oauth_login_state::set_confirmation`]). Both consent endpoints compare
//! the presented token with the stored one in constant time and answer the
//! 400 `Invalid state` for anything else; the `POST` deletes the state, so a
//! verdict can be given once.

use axum::{
    Json,
    http::{HeaderMap, header::CONTENT_TYPE},
    response::{IntoResponse as _, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq as _;
use uuid::Uuid;

use crate::{
    api::oauth::{
        authorize::{self, AuthorizeReply},
        json_media::is_json_media_type,
    },
    error::{AppError, AppResult},
    models::{
        oauth_login_state::{self, OauthLoginState},
        user::{self, User, register::RegisterError},
    },
    oauth::scopes::Scopes,
    state::AppState,
};

use super::{
    INVALID_STATE, REGISTRATIONS_CLOSED_FORM,
    form::RegisterForm,
    provider::granted,
    registration::{Created, create_account, registrable_state},
    state::StoredParams,
};

/// 400 body when the consent verdict carries no `approve`.
const MISSING_APPROVE: &str = "Missing parameter: approve";

/// The `pending` value of an account waiting for staff approval.
const PENDING_APPROVAL: &str = "approval";
/// The `pending` value of an account waiting for its email's confirmation.
const PENDING_CONFIRMATION: &str = "confirmation";

/// The `GET /oauth/external/consent` query.
#[derive(Debug, Deserialize)]
pub(super) struct ConsentQuery {
    /// The state the callback redirected with.
    #[serde(default)]
    pub(super) state: Option<String>,
    /// The one-time token the callback redirected with.
    #[serde(default)]
    pub(super) confirm_token: Option<String>,
}

/// The `POST /oauth/external/consent` body: the verdict on the login.
#[derive(Deserialize)]
pub(super) struct ConsentBody {
    /// The state the callback redirected with.
    #[serde(default)]
    state: Option<String>,
    /// The one-time token the callback redirected with. Never in `Debug`
    /// output.
    #[serde(default)]
    confirm_token: Option<String>,
    /// The user's verdict: `true` mints the code, `false` denies.
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    approve: Option<bool>,
}

/// The `confirmation` document a callback leaves in the state of a linked
/// login that needs consent.
#[derive(Deserialize)]
struct Confirmation {
    /// The one-time `confirm_token`.
    token: String,
    /// The linked local user the code is minted for.
    user_id: Uuid,
}

impl Confirmation {
    /// The `confirmation` document of a live state, or `None` when the
    /// state carries none or one without a token.
    fn of_state(stored: &OauthLoginState) -> Option<Self> {
        let raw = stored.params.get("confirmation")?;
        let found: Self = serde_json::from_value(raw.clone()).ok()?;
        (!found.token.is_empty()).then_some(found)
    }
}

/// Whether the request body is declared `application/json`: what tells the
/// frontend's registration from the HTML form's.
pub(super) fn is_json_body(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_json_media_type)
}

/// The response of a JSON endpoint: 200 with the body, or the error.
pub(super) fn json_response(outcome: AppResult<Value>) -> Response {
    match outcome {
        Ok(body) => Json(body).into_response(),
        Err(err) => err.into_response(),
    }
}

/// `GET /oauth/registration_details` as JSON: the provider's account info
/// held by the state, `null` where the provider said nothing.
///
/// # Errors
///
/// 400 `Invalid state` from [`registrable_state`]; any database failure
/// arrives as [`AppError::Database`].
pub(super) async fn details_json(state: &AppState, presented: Option<&str>) -> AppResult<Value> {
    let (_, pending, _) = registrable_state(state, presented).await?;
    Ok(json!({
        "nickname": pending.nickname,
        "email": pending.email,
        "name": pending.name,
        "provider": pending.provider,
    }))
}

/// `POST /oauth/register` as JSON: the account created exactly as the form
/// creates it, and the outcome as JSON (see the module docs).
///
/// # Errors
///
/// 400 `Invalid state`, 403 while registrations are closed, 422 for a
/// refused submission, or any database or key generation failure of the
/// registration itself as [`AppError`].
pub(super) async fn registered_json(state: &AppState, form: RegisterForm) -> AppResult<Value> {
    let (stored, pending, params) = registrable_state(state, form.state.as_deref()).await?;
    let created = create_account(state, &form, &stored, &pending, form.password.as_deref()).await?;
    match created {
        Created::Closed => Err(AppError::Forbidden(REGISTRATIONS_CLOSED_FORM.to_owned())),
        Created::Invalid(errors) => Err(RegisterError::Invalid(*errors).into()),
        Created::Done(user) => finished_json(state, &user, &params).await,
    }
}

/// What a finished registration is answered with: which wait the account
/// is in — approval weighed first, as the HTML page does — or the login.
async fn finished_json(state: &AppState, user: &User, params: &StoredParams) -> AppResult<Value> {
    if user.approval_pending.get() {
        return Ok(json!({ "pending": PENDING_APPROVAL }));
    }
    if !user.email_confirmed.get() {
        return Ok(json!({ "pending": PENDING_CONFIRMATION }));
    }
    granted_json(state, user, params).await
}

/// The login a finished flow earned, as JSON: the browser flow's own
/// checks, MFA detour and code ([`granted`]), answered by [`reply_json`].
///
/// # Errors
///
/// As [`granted`]'s.
async fn granted_json(state: &AppState, user: &User, params: &StoredParams) -> AppResult<Value> {
    reply_json(granted(state, user, params).await?)
}

/// A finished login's reply as JSON: a redirect as
/// `{"redirect_uri": <its Location>}`, an out-of-band code as
/// `{"code": …}`.
///
/// # Errors
///
/// [`AppError::Internal`] for a reply that is a page, which no finished
/// login answers.
fn reply_json(reply: AuthorizeReply) -> AppResult<Value> {
    match reply {
        AuthorizeReply::Found(location) => {
            let url = location.to_str().map_err(|_unreadable| {
                AppError::BadRequest(authorize::INVALID_REDIRECT_URI.to_owned())
            })?;
            Ok(json!({ "redirect_uri": url }))
        }
        AuthorizeReply::Code(code) => Ok(json!({ "code": code })),
        AuthorizeReply::Page(_) => Err(AppError::Internal(
            "an external login answered a page where a redirect belongs".to_owned(),
        )),
    }
}

/// The state a consent endpoint may run on: live, parked for consent, and
/// presented with its own `confirm_token` — compared in constant time.
///
/// # Errors
///
/// 400 `Invalid state` for a missing, unknown or expired state, one not
/// parked for consent, unreadable stored parameters or a wrong token; any
/// database failure arrives as [`AppError::Database`].
async fn consenting_state(
    state: &AppState,
    presented: Option<&str>,
    token: Option<&str>,
) -> AppResult<(OauthLoginState, StoredParams, Confirmation)> {
    let invalid = || AppError::BadRequest(INVALID_STATE.to_owned());
    let Some(key) = presented.filter(|text| !text.is_empty()) else {
        return Err(invalid());
    };
    let stored = oauth_login_state::get_live(&state.db, key)
        .await?
        .ok_or_else(invalid)?;
    let confirmation = Confirmation::of_state(&stored).ok_or_else(invalid)?;
    let offered = token.unwrap_or_default();
    if !bool::from(offered.as_bytes().ct_eq(confirmation.token.as_bytes())) {
        return Err(invalid());
    }
    let params = StoredParams::of_state(&stored)?;
    Ok((stored, params, confirmation))
}

/// `GET /oauth/external/consent`: the application asking for the login and
/// the scopes it asks for, for the frontend's consent screen.
///
/// # Errors
///
/// 400 `Invalid state` (see [`consenting_state`]) or the 400 of a stored
/// application that no longer exists.
pub(super) async fn consent_details(state: &AppState, query: ConsentQuery) -> AppResult<Value> {
    let (_, params, _) = consenting_state(
        state,
        query.state.as_deref(),
        query.confirm_token.as_deref(),
    )
    .await?;
    let app = authorize::load_app(&state.db, Some(params.client_id.as_str())).await?;
    Ok(json!({
        "app": { "name": app.client_name, "website": app.website },
        "scopes": Scopes::parse(&params.scope).names(),
    }))
}

/// `POST /oauth/external/consent`: the user's verdict. The state is deleted
/// before anything is minted, so one state gives one verdict; a denial
/// answers `{"redirect_uri": …}` with `error=access_denied`, an approval
/// the login as [`registered_json`]'s success does.
///
/// # Errors
///
/// 400 `Invalid state` (see [`consenting_state`], also for a state a
/// concurrent verdict took first or whose user is gone), 400 for a missing
/// `approve`, and the refusals of the login itself ([`granted`]).
pub(super) async fn consent_decided(state: &AppState, body: ConsentBody) -> AppResult<Value> {
    let (stored, params, confirmation) =
        consenting_state(state, body.state.as_deref(), body.confirm_token.as_deref()).await?;
    let Some(approved) = body.approve else {
        return Err(AppError::BadRequest(MISSING_APPROVE.to_owned()));
    };
    if !oauth_login_state::delete(&state.db, &stored.state).await? {
        return Err(AppError::BadRequest(INVALID_STATE.to_owned()));
    }
    if !approved {
        return reply_json(authorize::denied(&params.delivery())?);
    }
    let linked = user::get_by_id(&state.db, confirmation.user_id)
        .await?
        .ok_or_else(|| AppError::BadRequest(INVALID_STATE.to_owned()))?;
    granted_json(state, &linked, &params).await
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use chrono::Utc;

    use super::*;

    /// A state row whose parameters are `params`.
    fn row(params: Value) -> OauthLoginState {
        OauthLoginState {
            state: "a-state".to_owned(),
            provider: "github".to_owned(),
            params,
            valid_until: Utc::now(),
        }
    }

    #[test]
    fn the_confirmation_reads_back_out_of_a_state_row() {
        let user_id = Uuid::now_v7();
        let found = Confirmation::of_state(&row(json!({
            "client_id": "c",
            "confirmation": {"token": "t", "user_id": user_id},
        })))
        .expect("the callback's document reads");
        assert_eq!(found.token, "t", "the token");
        assert_eq!(found.user_id, user_id, "and the linked user");

        for unusable in [
            json!({"client_id": "c"}),
            json!({"confirmation": null}),
            json!({"confirmation": "nope"}),
            json!({"confirmation": {"token": "", "user_id": user_id}}),
            json!({"confirmation": {"token": "t"}}),
            json!({"confirmation": {"token": "t", "user_id": "not-a-uuid"}}),
        ] {
            assert!(
                Confirmation::of_state(&row(unusable.clone())).is_none(),
                "`{unusable}` parks nothing for consent"
            );
        }
    }

    #[test]
    fn only_a_json_content_type_is_a_json_body() {
        let with = |value: &'static str| {
            let mut headers = HeaderMap::new();
            let _absent = headers.insert(CONTENT_TYPE, HeaderValue::from_static(value));
            headers
        };
        assert!(is_json_body(&with("application/json")), "plain");
        assert!(
            is_json_body(&with("Application/JSON; charset=utf-8")),
            "any case, with parameters"
        );
        assert!(
            !is_json_body(&with("application/x-www-form-urlencoded")),
            "a form is not"
        );
        assert!(!is_json_body(&HeaderMap::new()), "no header, no JSON");
    }

    #[test]
    fn a_redirect_is_the_location_a_code_is_the_code_and_a_page_is_a_fault() {
        let found = reply_json(AuthorizeReply::Found(HeaderValue::from_static(
            "https://client.example/cb?code=c",
        )))
        .expect("a redirect answers");
        assert_eq!(
            found,
            json!({"redirect_uri": "https://client.example/cb?code=c"}),
            "the Location, under `redirect_uri`"
        );
        let code = reply_json(AuthorizeReply::Code("oob-code".to_owned()))
            .expect("an out-of-band code answers");
        assert_eq!(
            code,
            json!({"code": "oob-code"}),
            "the out-of-band code, under `code`"
        );
        assert!(
            reply_json(AuthorizeReply::Page("<p>x</p>".to_owned())).is_err(),
            "a page has no redirect to give"
        );
    }

    #[test]
    fn a_missing_verdict_reads_as_none() {
        let body: ConsentBody = crate::extract::into_params(json!({"state": "s"}))
            .expect("every field is optional at the shape level");
        assert_eq!(body.approve, None, "no verdict is no verdict");
        let denied: ConsentBody = crate::extract::into_params(json!({"approve": "false"}))
            .expect("the string spelling reads too");
        assert_eq!(denied.approve, Some(false), "a denial");
    }
}
