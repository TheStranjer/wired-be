//! The registration half of an external login: the write
//! `POST /oauth/register` does ([`create_account`], with what it came to as
//! [`Created`]) and the state the flow may run on ([`registrable_state`]).
//! The JSON answers are the sibling `frontend` child's; the submission's own
//! data is the sibling `form` child's.

use tracing::debug;

use crate::{
    error::{AppError, AppResult},
    models::{
        oauth_login_state::{self, OauthLoginState},
        user::{
            User,
            register::{FieldErrors, RegisterError, create_local_user},
        },
        user_identity,
    },
    state::AppState,
};

use super::{
    INVALID_STATE,
    form::{PendingRegistration, RegisterForm, Submitted},
    state::StoredParams,
};

/// What creating the account of a submitted registration came to. The
/// payloads are boxed so the variants stay the size of a pointer.
#[derive(Debug)]
pub(super) enum Created {
    /// `REGISTRATIONS_OPEN` is off: nothing was written.
    Closed,
    /// Validation or an availability check refused the submission; nothing
    /// was written.
    Invalid(Box<FieldErrors>),
    /// The account exists, the provider identity is linked and the state
    /// row is spent.
    Done(Box<User>),
}

/// The write of the registration: the `REGISTRATIONS_OPEN` gate, the
/// account created through [`create_local_user`] from `form` resolved
/// against `pending` (with `chosen` as its password, or a random one nobody
/// can type), the identity link, and the burned state row.
///
/// # Errors
///
/// Any database or key generation failure of the registration itself as
/// [`AppError`]; a refusal is [`Created::Closed`] or [`Created::Invalid`].
pub(super) async fn create_account(
    state: &AppState,
    form: &RegisterForm,
    stored: &OauthLoginState,
    pending: &PendingRegistration,
    chosen: Option<&str>,
) -> AppResult<Created> {
    if !state.config.registrations_open.get() {
        return Ok(Created::Closed);
    }
    let account = Submitted::of_form(form, pending);
    let user = match create_local_user(state, account.into_register_params(chosen), None).await {
        Ok(user) => user,
        Err(RegisterError::Invalid(errors)) => return Ok(Created::Invalid(Box::new(errors))),
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
    Ok(Created::Done(Box::new(user)))
}

/// The state the registration may run on: live under its own key,
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

#[cfg(test)]
mod tests {
    use axum::http::{HeaderValue, Request, StatusCode, header};
    use tower::ServiceExt as _;

    use crate::api::oauth_external::{
        router,
        test_support::{header_of, test_state},
    };

    #[tokio::test]
    async fn the_registration_routes_refuse_a_stateless_request_as_json_400() {
        let get = router()
            .with_state(test_state(&["github"]))
            .oneshot(
                Request::get("/oauth/registration_details")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .expect("the router answers in process");
        assert_eq!(
            get.status(),
            StatusCode::BAD_REQUEST,
            "no state, no details"
        );
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
        assert_eq!(
            header_of(&post, header::CONTENT_TYPE).as_deref(),
            Some("application/json"),
            "a form-encoded submission is refused as JSON too"
        );
    }
}
