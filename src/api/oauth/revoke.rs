//! `POST /oauth/revoke`: retiring one of the application's access
//! tokens (Pleroma's `token_revoke`, RFC 7009). The endpoint shares the
//! parameter handling and application authentication of `POST
//! /oauth/token` — [`validate_app`](super::validate_app) and
//! [`invalid_credentials`](super::params::invalid_credentials) — and owns
//! [`no_cache_headers`], the no-store stamp every `/oauth` credential
//! answer carries (RFC 6749 §5.1).

use std::fmt;

use axum::{
    Json,
    extract::State,
    http::{
        HeaderMap, HeaderValue,
        header::{CACHE_CONTROL, PRAGMA},
    },
    response::{IntoResponse as _, Response},
};
use serde::{Deserialize, Serialize};

use super::{
    MISSING_TOKEN, NO_CACHE, NO_STORE,
    params::{invalid_credentials, validate_app},
};
use crate::{
    error::{AppError, AppResult},
    extract::ApiBody,
    models::oauth_token,
    state::AppState,
};

/// `POST /oauth/revoke` — retire one of the application's access tokens
/// (Pleroma's `token_revoke`, RFC 7009). No bearer authentication: the
/// application authenticates with its client credentials, like on
/// `/oauth/token`. Every answer, success or error, gets the no-store
/// headers.
pub(super) async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiBody(params): ApiBody<RevokeParams>,
) -> Response {
    let response = match revoke_token(&state, &headers, &params).await {
        Ok(body) => body.into_response(),
        Err(err) => err.into_response(),
    };
    no_cache_headers(response)
}

/// The revocation, in Pleroma's order: the `token` parameter first (its
/// pattern match precedes the credential check), then the same
/// application authentication as [`token`](super::token), then the lookup. A token
/// issued to another application is refused like a credential failure
/// (400 [`INVALID_CREDENTIALS`](super::INVALID_CREDENTIALS)); an unknown token revokes to nothing
/// without an error, since its invalidation is already achieved
/// (RFC 7009 §2.2).
///
/// # Errors
///
/// 400 [`MISSING_TOKEN`] when the parameter is absent, 400
/// [`INVALID_CREDENTIALS`](super::INVALID_CREDENTIALS) for a failed app authentication or a foreign
/// token; database failures arrive as [`AppError::Database`].
async fn revoke_token(
    state: &AppState,
    headers: &HeaderMap,
    params: &RevokeParams,
) -> AppResult<Json<RevokeResponse>> {
    let Some(token) = params.token.as_deref() else {
        return Err(AppError::BadRequest(MISSING_TOKEN.to_owned()));
    };
    let app = validate_app(state, headers, params.body_credentials()).await?;
    if let Some((found, owner)) = oauth_token::get_with_app(&state.db, token).await? {
        if owner.id != app.id {
            return Err(invalid_credentials());
        }
        oauth_token::delete_token(&state.db, found.id).await?;
    }
    Ok(Json(RevokeResponse))
}

/// The request parameters of `POST /oauth/revoke`: the access token to
/// retire and the client credentials, the latter presented either here
/// or through HTTP Basic. As in [`TokenParams`](super::TokenParams) every field is optional
/// at the shape level, so the handler answers Pleroma's own
/// missing-parameter message instead of serde rejecting the body. The
/// manual `Debug` impl (see below) redacts the two secret fields.
#[derive(Deserialize)]
pub(super) struct RevokeParams {
    /// The access token to revoke; absent or null is the 400
    /// [`MISSING_TOKEN`]. Renders as `<redacted>` in `Debug` output.
    #[serde(default)]
    token: Option<String>,
    /// The public OAuth client id, when not presented through HTTP Basic.
    #[serde(default)]
    client_id: Option<String>,
    /// The OAuth client secret, when not presented through HTTP Basic.
    /// Never in `Debug` output.
    #[serde(default)]
    client_secret: Option<String>,
}

/// [`token`](Self::token) and [`client_secret`](Self::client_secret) are
/// secret material and render as `<redacted>`; the client id shows.
impl fmt::Debug for RevokeParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RevokeParams")
            .field("token", &"<redacted>")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

impl RevokeParams {
    /// The `client_id`/`client_secret` body fields as a pair, `None`
    /// unless both are present.
    fn body_credentials(&self) -> Option<(&str, &str)> {
        Some((self.client_id.as_deref()?, self.client_secret.as_deref()?))
    }
}

/// The 200 body of a successful revocation: the empty JSON object, what
/// Pleroma's `token_revoke` renders (`json(conn, %{})`); the client is
/// meant to ignore the body of a revocation answer (RFC 7009 §2.2).
/// `Serialize` is manual because a derived unit struct would render
/// `null`, not `{}`.
#[derive(Debug)]
struct RevokeResponse;

impl Serialize for RevokeResponse {
    /// Writes the empty JSON object by serializing an empty map.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let empty = std::collections::BTreeMap::<String, serde_json::Value>::new();
        empty.serialize(serializer)
    }
}

/// Stamps the cache headers every token answer carries: issued
/// credentials must never be stored (RFC 6749 §5.1).
pub(super) fn no_cache_headers(mut response: Response) -> Response {
    let stored_cache_control = response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static(NO_STORE));
    drop(stored_cache_control);
    let stored_pragma = response
        .headers_mut()
        .insert(PRAGMA, HeaderValue::from_static(NO_CACHE));
    drop(stored_pragma);
    response
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        api::oauth::{
            INVALID_CREDENTIALS,
            test_support::{message_of, test_state},
        },
        error::AppError,
    };

    /// [`RevokeParams`] with the three fields as given.
    fn revoke_params(
        token: Option<&str>,
        client_id: Option<&str>,
        secret: Option<&str>,
    ) -> RevokeParams {
        RevokeParams {
            token: token.map(str::to_owned),
            client_id: client_id.map(str::to_owned),
            client_secret: secret.map(str::to_owned),
        }
    }

    #[test]
    fn revoke_body_credentials_need_both_fields() {
        assert_eq!(
            revoke_params(Some("tok"), None, None).body_credentials(),
            None,
            "nothing sent"
        );
        assert_eq!(
            revoke_params(Some("tok"), Some("id"), None).body_credentials(),
            None,
            "an id without a secret"
        );
        assert_eq!(
            revoke_params(Some("tok"), Some("id"), Some("secret")).body_credentials(),
            Some(("id", "secret")),
            "the pair completes"
        );
    }

    #[test]
    fn revoke_params_debug_redacts_the_token_and_client_secret() {
        let params = revoke_params(
            Some("access-secret"),
            Some("client-id"),
            Some("client-secret"),
        );
        let debug = format!("{params:?}");
        assert!(
            !debug.contains("access-secret") && !debug.contains("client-secret"),
            "the secrets must not leak: {debug}"
        );
        assert!(
            debug.contains(r#"token: "<redacted>""#)
                && debug.contains(r#"client_secret: "<redacted>""#),
            "the markers name their fields: {debug}"
        );
        assert!(debug.contains("client-id"), "the public id shows");
    }

    #[test]
    fn revoke_json_and_form_shapes_deserialize_to_the_same_params() {
        let full: RevokeParams = crate::extract::into_params(json!({
            "token": "a-token",
            "client_id": "id",
            "client_secret": "secret",
        }))
        .unwrap();
        assert_eq!(full.token.as_deref(), Some("a-token"), "the token");
        assert_eq!(full.client_id.as_deref(), Some("id"), "the client id");
        // A form body parses to the same string-shaped map (the form
        // keys themselves are covered by `crate::extract`'s tests).
        let form: RevokeParams = crate::extract::into_params(json!({
            "token": "a-token",
            "client_id": "id",
        }))
        .unwrap();
        assert_eq!(form.token.as_deref(), Some("a-token"), "same token");
        assert_eq!(form.client_secret, None, "an absent field stays absent");
    }

    #[test]
    fn a_successful_revoke_answers_the_empty_object() {
        assert_eq!(
            serde_json::to_value(RevokeResponse).unwrap(),
            json!({}),
            "Pleroma renders the empty JSON object"
        );
    }

    #[test]
    fn every_answer_gets_the_no_store_headers() {
        let error_response =
            no_cache_headers(AppError::BadRequest(INVALID_CREDENTIALS.to_owned()).into_response());
        let ok_response = no_cache_headers(Json(json!({"token_type": "Bearer"})).into_response());
        for response in [error_response, ok_response] {
            assert_eq!(
                response.headers().get(CACHE_CONTROL),
                Some(&HeaderValue::from_static(NO_STORE)),
                "cache-control: no-store"
            );
            assert_eq!(
                response.headers().get(PRAGMA),
                Some(&HeaderValue::from_static(NO_CACHE)),
                "pragma: no-cache"
            );
        }
    }

    #[tokio::test]
    async fn a_revoke_without_the_token_parameter_is_refused_by_name() {
        // The parameter gate runs before the credential check, and the
        // credential check before any database access, so the lazy pool
        // of `test_state` is never consulted.
        let err = revoke_token(
            &test_state(),
            &HeaderMap::new(),
            &revoke_params(None, None, None),
        )
        .await
        .expect_err("no token is no revocation");
        assert_eq!(
            message_of(&err),
            MISSING_TOKEN,
            "the refusal names the missing parameter even without credentials"
        );
    }
}
