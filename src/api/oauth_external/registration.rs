//! The registration half of an external login: what `POST /oauth/register`
//! does ([`registered`]), what a finished registration is answered with
//! ([`finish_registration`]), the state the flow may run on
//! ([`registrable_state`]), and the two ways the form is rendered — fresh
//! from the provider's account ([`pending_form`]) or re-rendered above a
//! refusal ([`refused_form`]), with [`validation_line`] spelling that
//! refusal. The form's own data is the sibling `form` child's.

use tracing::debug;

use crate::{
    api::{
        accounts::{REGISTERED_AWAITING_APPROVAL, REGISTERED_AWAITING_CONFIRMATION},
        oauth::authorize::AuthorizeReply,
    },
    error::{AppError, AppResult},
    models::{
        oauth_login_state::{self, OauthLoginState},
        user::{
            User,
            register::{FieldErrors, RegisterError, create_local_user},
        },
        user_identity,
    },
    oauth::registration_page,
    state::AppState,
};

use super::{
    INVALID_STATE, REGISTRATIONS_CLOSED_FORM,
    form::{PendingRegistration, RegisterForm, Submitted},
    provider::granted,
    state::StoredParams,
};

/// The registration half in order: the state that names the pending
/// account and the stored authorize parameters, the gates, the account,
/// the link, and the login the state was prepared for.
///
/// # Errors
///
/// 400 `Invalid state` (see [`registrable_state`]), or any database or key
/// generation failure of the registration itself as [`AppError`].
pub(super) async fn registered(state: &AppState, form: RegisterForm) -> AppResult<AuthorizeReply> {
    let (stored, pending, params) = registrable_state(state, form.state.as_deref()).await?;
    if !state.config.registrations_open.get() {
        return Ok(refused_form(
            &form,
            &stored.state,
            REGISTRATIONS_CLOSED_FORM,
        ));
    }
    let account = Submitted::of_form(&form, &pending);
    let user = match create_local_user(state, account.into_register_params(), None).await {
        Ok(user) => user,
        Err(RegisterError::Invalid(errors)) => {
            return Ok(refused_form(
                &form,
                &stored.state,
                &validation_line(&errors),
            ));
        }
        Err(RegisterError::Failed(err)) => return Err(err),
    };
    let linked = user_identity::link(&state.db, user.id, &pending.provider, &pending.uid).await?;
    let _finished = oauth_login_state::delete(&state.db, &stored.state).await?;
    debug!(
        user = %user.id,
        identity = %linked.id,
        provider = %pending.provider,
        "external registration linked a new account and finished the login"
    );
    finish_registration(state, &user, &params).await
}

/// What a finished external registration is answered with: the message
/// page of an account that first waits for staff approval or for its
/// email's confirmation — the same two messages `POST /api/v1/accounts`
/// answers with, approval weighed first — or, for a ready account, the
/// very login the callback gives a linked user.
async fn finish_registration(
    state: &AppState,
    user: &User,
    params: &StoredParams,
) -> AppResult<AuthorizeReply> {
    if user.approval_pending.get() {
        return Ok(AuthorizeReply::Page(registration_page::render_message(
            REGISTERED_AWAITING_APPROVAL,
        )));
    }
    if !user.email_confirmed.get() {
        return Ok(AuthorizeReply::Page(registration_page::render_message(
            REGISTERED_AWAITING_CONFIRMATION,
        )));
    }
    granted(state, user, params).await
}

/// The state the registration page may run on: live under its own key,
/// carrying readable authorize parameters *and* the `pending` account an
/// unlinked callback left. The registration flow does not know (or care)
/// which provider issued it — the row says, and the `pending` document is
/// the write's guarantee that it was this server that stored it.
///
/// # Errors
///
/// 400 `Invalid state` for a missing, unknown or expired state, or one
/// that carries no usable `pending`; the same 400 for unreadable stored
/// parameters. Any database failure arrives as [`AppError::Database`].
pub(super) async fn registrable_state(
    state: &AppState,
    presented: Option<&str>,
) -> AppResult<(OauthLoginState, PendingRegistration, StoredParams)> {
    let invalid = || AppError::BadRequest(INVALID_STATE.to_owned());
    let Some(presented) = presented.filter(|text| !text.is_empty()) else {
        return Err(invalid());
    };
    let stored = oauth_login_state::get_live(&state.db, presented)
        .await?
        .ok_or_else(invalid)?;
    let params = StoredParams::of_state(&stored)?;
    let pending = PendingRegistration::of_state(&stored).ok_or_else(invalid)?;
    Ok((stored, pending, params))
}

/// The registration form as the `GET` renders it: the provider's account
/// info prefilled — blank where the provider said nothing — and no error.
pub(super) fn pending_form(
    stored: &OauthLoginState,
    pending: &PendingRegistration,
) -> AuthorizeReply {
    AuthorizeReply::Page(registration_page::render_form(
        &registration_page::RegistrationForm {
            nickname: pending.nickname.as_deref().unwrap_or_default(),
            email: pending.email.as_deref().unwrap_or_default(),
            name: pending.name.as_deref().unwrap_or_default(),
            bio: "",
            state: &stored.state,
            error: None,
        },
    ))
}

/// The registration form as a refused submission re-renders it: what the
/// user typed, kept, with `error` above the form.
fn refused_form(form: &RegisterForm, state: &str, error: &str) -> AuthorizeReply {
    AuthorizeReply::Page(registration_page::render_form(
        &registration_page::RegistrationForm {
            nickname: form.nickname.as_deref().unwrap_or_default(),
            email: form.email.as_deref().unwrap_or_default(),
            name: form.name.as_deref().unwrap_or_default(),
            bio: form.bio.as_deref().unwrap_or_default(),
            state,
            error: Some(error),
        },
    ))
}

/// Every field-validation failure of a refused registration in one line:
/// Pleroma's `register` action joins its changeset errors the same way,
/// `"field message; field message"`.
fn validation_line(errors: &FieldErrors) -> String {
    errors
        .iter()
        .map(|(field, messages)| format!("{field} {}", messages.join(", ")))
        .collect::<Vec<String>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderValue, Request, StatusCode, header};
    use chrono::Utc;
    use serde_json::json;
    use tower::ServiceExt as _;

    use super::*;
    use crate::api::oauth_external::{
        router,
        test_support::{header_of, test_state},
    };

    #[tokio::test]
    async fn the_registration_pages_refuse_a_stateless_request_as_400() {
        let get = router()
            .with_state(test_state(&["github"]))
            .oneshot(
                Request::get("/oauth/registration_details")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .expect("the router answers in process");
        assert_eq!(get.status(), StatusCode::BAD_REQUEST, "no state, no form");
        assert_eq!(
            header_of(&get, header::CONTENT_TYPE).as_deref(),
            Some("application/json"),
            "the refusal is JSON"
        );
        let post = router()
            .with_state(test_state(&["github"]))
            .oneshot(
                Request::post("/oauth/register")
                    .header(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/x-www-form-urlencoded"),
                    )
                    .body(axum::body::Body::from("state=&nickname=octo"))
                    .unwrap(),
            )
            .await
            .expect("the router answers in process");
        assert_eq!(
            post.status(),
            StatusCode::BAD_REQUEST,
            "an empty state submits to nothing"
        );
    }

    #[test]
    fn the_field_errors_render_as_one_line_above_the_form() {
        let mut errors = FieldErrors::new();
        let _new = errors.insert("nickname", vec!["has already been taken".to_owned()]);
        assert_eq!(
            validation_line(&errors),
            "nickname has already been taken",
            "Pleroma's `field message` spelling"
        );
        let _prior = errors.insert(
            "email",
            vec!["can't be blank".to_owned(), "has invalid format".to_owned()],
        );
        assert_eq!(
            validation_line(&errors),
            "email can't be blank, has invalid format; nickname has already been taken",
            "every field, messages joined, fields ordered and separated"
        );
    }

    #[test]
    fn the_refused_form_keeps_the_submission_and_names_the_error() {
        let form: RegisterForm = crate::extract::into_params(json!({
            "state": "s",
            "nickname": "kept",
            "email": "a&b@example.org",
            "bio": "</textarea><p>x</p>",
        }))
        .expect("a submission");
        let AuthorizeReply::Page(html) =
            refused_form(&form, "s", "nickname has already been taken")
        else {
            panic!("a refused registration re-renders a page, never a redirect")
        };
        assert!(html.contains("value=\"kept\""), "what the user typed stays");
        assert!(
            html.contains("value=\"a&amp;b@example.org\""),
            "escaped, as ever"
        );
        assert!(
            !html.contains("</textarea><p>x</p>"),
            "a bio cannot close its textarea: {html}"
        );
        assert!(
            html.contains("role=\"alert\">nickname has already been taken</p>"),
            "the error line stands above the form"
        );
        assert!(
            html.contains("<input type=\"hidden\" name=\"state\" value=\"s\">"),
            "and the state rides along for the retry"
        );
    }

    #[test]
    fn the_pending_form_prefills_the_provider_and_blanks_the_rest() {
        let row = OauthLoginState {
            state: "issued".to_owned(),
            provider: "github".to_owned(),
            params: json!({"client_id": "c"}),
            valid_until: Utc::now(),
        };
        let pending = PendingRegistration {
            provider: "github".to_owned(),
            uid: "4242".to_owned(),
            nickname: Some("octocat".to_owned()),
            email: None,
            name: Some("a\"b".to_owned()),
        };
        let AuthorizeReply::Page(html) = pending_form(&row, &pending) else {
            panic!("a form is a page")
        };
        assert!(
            html.contains("name=\"nickname\" value=\"octocat\""),
            "the provider's nickname prefills"
        );
        assert!(
            html.contains("name=\"email\" value=\"\""),
            "a provider that said no email renders a blank field"
        );
        assert!(
            html.contains("name=\"name\" value=\"a&quot;b\""),
            "and the name is escaped in its attribute"
        );
        assert!(
            html.contains("<input type=\"hidden\" name=\"state\" value=\"issued\">"),
            "the state goes out hidden"
        );
        assert!(
            !html.contains("role=\"alert\""),
            "a fresh form has no error"
        );
    }
}
