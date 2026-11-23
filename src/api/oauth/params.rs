//! The request half of `POST /oauth/token`: the parameter bag both token
//! and revocation endpoints deserialize, the grant gate that picks the
//! [`Grant`](super::Grant), the application authentication the whole of
//! `/oauth` shares, and the scope bounding every user-bound and
//! app-only issuance runs on. The grant bodies themselves live in
//! [`super::grants`].

use std::fmt;

use axum::http::{HeaderMap, header::AUTHORIZATION};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use subtle::ConstantTimeEq as _;

use super::{
    BASIC_SCHEME, GRANT_TYPE_AUTHORIZATION_CODE, GRANT_TYPE_CLIENT_CREDENTIALS,
    GRANT_TYPE_PASSWORD, GRANT_TYPE_REFRESH_TOKEN, Grant, INVALID_CREDENTIALS, INVALID_SCOPE,
    MISSING_GRANT_TYPE, UNSUPPORTED_GRANT_TYPE,
};
use crate::{
    error::{AppError, AppResult},
    models::oauth_app::{self, OauthApp},
    oauth::scopes::Scopes,
    state::AppState,
};

/// The request parameters of `POST /oauth/token`.
///
/// Every field is optional at the shape level: Pleroma answers the
/// missing and the unknown with its own messages (`"Missing parameter:
/// grant_type"`, `"Invalid credentials"`), so the handler checks the
/// fields instead of serde rejecting the body. The `Debug` impl is manual
/// (see below) because two fields are secret material.
#[derive(Deserialize)]
pub(super) struct TokenParams {
    /// Which grant to run; [`GRANT_TYPE_PASSWORD`],
    /// [`GRANT_TYPE_CLIENT_CREDENTIALS`], [`GRANT_TYPE_REFRESH_TOKEN`] and
    /// [`GRANT_TYPE_AUTHORIZATION_CODE`] are implemented.
    #[serde(default)]
    pub(super) grant_type: Option<String>,
    /// The account's nickname or email. Required by the password grant.
    #[serde(default)]
    pub(super) username: Option<String>,
    /// The account's password. Required by the password grant. Renders as
    /// `<redacted>` in `Debug` output.
    #[serde(default)]
    pub(super) password: Option<String>,
    /// Requested scopes, space- or `+`-separated; absent or blank means
    /// [`Scopes::default`] (`read`).
    #[serde(default)]
    pub(super) scope: Option<String>,
    /// The public OAuth client id, when not presented through HTTP Basic.
    #[serde(default)]
    pub(super) client_id: Option<String>,
    /// The OAuth client secret, when not presented through HTTP Basic.
    /// Never in `Debug` output.
    #[serde(default)]
    pub(super) client_secret: Option<String>,
    /// The refresh token to rotate. Required by the `refresh_token`
    /// grant. Renders as `<redacted>` in `Debug` output.
    #[serde(default)]
    pub(super) refresh_token: Option<String>,
    /// The authorization code to redeem. Required by the
    /// `authorization_code` grant. Renders as `<redacted>` in `Debug`
    /// output: it is a one-time credential.
    #[serde(default)]
    pub(super) code: Option<String>,
    /// The redirect URI the code was issued for. The `authorization_code`
    /// grant must present it when the code is bound to one (RFC 6749
    /// §4.1.3).
    #[serde(default)]
    pub(super) redirect_uri: Option<String>,
}

/// [`password`](Self::password), [`client_secret`](Self::client_secret),
/// [`refresh_token`](Self::refresh_token) and [`code`](Self::code) are
/// secret material and render as `<redacted>`; everything else shows.
impl fmt::Debug for TokenParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenParams")
            .field("grant_type", &self.grant_type)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("scope", &self.scope)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("code", &"<redacted>")
            .field("redirect_uri", &self.redirect_uri)
            .finish()
    }
}

impl TokenParams {
    /// The `client_id`/`client_secret` body fields as a pair, `None`
    /// unless both are present.
    pub(super) fn body_credentials(&self) -> Option<(&str, &str)> {
        Some((self.client_id.as_deref()?, self.client_secret.as_deref()?))
    }
}

/// Checks the grant type and names the grant to run. The password,
/// `client_credentials`, `refresh_token` and `authorization_code` grants
/// are the ones implemented here.
///
/// # Errors
///
/// 400 [`MISSING_GRANT_TYPE`] when the field is absent, 400
/// [`UNSUPPORTED_GRANT_TYPE`] for any value other than
/// [`GRANT_TYPE_PASSWORD`], [`GRANT_TYPE_CLIENT_CREDENTIALS`],
/// [`GRANT_TYPE_REFRESH_TOKEN`] or [`GRANT_TYPE_AUTHORIZATION_CODE`].
pub(super) fn require_grant(params: &TokenParams) -> AppResult<Grant> {
    let Some(grant_type) = params.grant_type.as_deref() else {
        return Err(AppError::BadRequest(MISSING_GRANT_TYPE.to_owned()));
    };
    match grant_type {
        GRANT_TYPE_PASSWORD => Ok(Grant::Password),
        GRANT_TYPE_CLIENT_CREDENTIALS => Ok(Grant::ClientCredentials),
        GRANT_TYPE_REFRESH_TOKEN => Ok(Grant::RefreshToken),
        GRANT_TYPE_AUTHORIZATION_CODE => Ok(Grant::AuthorizationCode),
        _ => Err(AppError::BadRequest(UNSUPPORTED_GRANT_TYPE.to_owned())),
    }
}

/// The registered application behind the presented client credentials,
/// the `Authorization: Basic` header winning over the `body_credentials`
/// pair (Pleroma's `Token.Utils.fetch_client_credentials`: "Per RFC 6749,
/// HTTP Basic is preferred to body params"). Both `/oauth/token` and
/// `/oauth/revoke` authenticate their application through here.
///
/// # Errors
///
/// 400 [`INVALID_CREDENTIALS`] when no pair is presented, the `client_id`
/// finds no application, or the `client_secret` does not match its row.
pub(super) async fn validate_app(
    state: &AppState,
    headers: &HeaderMap,
    body_credentials: Option<(&str, &str)>,
) -> AppResult<OauthApp> {
    let basic = basic_credentials(headers);
    let presented = basic
        .as_ref()
        .map(|(id, secret)| (id.as_str(), secret.as_str()))
        .or(body_credentials);
    let Some((client_id, client_secret)) = presented else {
        return Err(invalid_credentials());
    };
    let app = oauth_app::get_by_client_id(&state.db, client_id).await?;
    app.filter(|found| secret_matches(&found.client_secret, client_secret))
        .ok_or_else(invalid_credentials)
}

/// The client credentials pair of an `Authorization: Basic
/// base64(client_id:client_secret)` header: `None` when the header is
/// absent, uses another scheme, is not valid base64 or UTF-8, or holds no
/// `:` separator (Pleroma's `fetch_client_credentials` falls back to the
/// body fields in every one of those cases).
fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, encoded) = value.split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case(BASIC_SCHEME) {
        return None;
    }
    let decoded = STANDARD.decode(encoded.trim()).ok()?;
    let text = std::str::from_utf8(&decoded).ok()?;
    let (client_id, client_secret) = text.split_once(':')?;
    Some((client_id.to_owned(), client_secret.to_owned()))
}

/// Whether `presented` is the stored client secret, compared in constant
/// time so a rejection cannot leak how much of it matched.
fn secret_matches(stored: &str, presented: &str) -> bool {
    stored.as_bytes().ct_eq(presented.as_bytes()).into()
}

/// The one 400 every credential failure shares: an unknown application,
/// a wrong secret, an unknown user and a wrong password are not told
/// apart (Pleroma's `render_invalid_credentials_error`).
pub(super) fn invalid_credentials() -> AppError {
    AppError::BadRequest(INVALID_CREDENTIALS.to_owned())
}

/// The scopes to grant: the requested ones — [`Scopes::default`] (`read`)
/// when the field is absent or blank — bounded by the application's
/// registration.
///
/// # Errors
///
/// 400 [`INVALID_SCOPE`] when a requested scope is not covered by the
/// app's registered scopes ([`Scopes::is_subset_of`]).
pub(super) fn requested_scopes(params: &TokenParams, app: &OauthApp) -> AppResult<Scopes> {
    let requested = params
        .scope
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map_or_else(Scopes::default, Scopes::parse);
    if requested.is_subset_of(&Scopes::parse(&app.scopes)) {
        Ok(requested)
    } else {
        Err(AppError::BadRequest(INVALID_SCOPE.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use chrono::Utc;
    use serde_json::json;

    use super::*;
    use crate::{
        api::oauth::test_support::{message_of, params_with_grant},
        util::id::new_id,
    };

    /// A stored-looking app row registered with `scopes`.
    fn app_row(scopes: &str) -> OauthApp {
        let now = Utc::now();
        OauthApp {
            id: new_id(),
            client_name: "Client".to_owned(),
            redirect_uris: "https://client.example/callback".to_owned(),
            scopes: scopes.to_owned(),
            website: None,
            client_id: "client-id".to_owned(),
            client_secret: "client-secret".to_owned(),
            trusted: false,
            user_id: None,
            inserted_at: now,
            updated_at: now,
        }
    }

    /// An `authorization: value` [`HeaderMap`].
    fn headers_with(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let previous: Option<HeaderValue> =
            headers.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        assert!(previous.is_none(), "a fresh map holds no header");
        headers
    }

    #[test]
    fn a_missing_grant_type_is_pleromas_missing_parameter_error() {
        let err = require_grant(&params_with_grant(None)).unwrap_err();
        assert_eq!(
            message_of(&err),
            MISSING_GRANT_TYPE,
            "an absent grant_type is refused by name"
        );
    }

    #[test]
    fn an_unknown_grant_type_is_unsupported() {
        for grant in ["urn:ietf:params:oauth:grant-type:device_code", "bogus"] {
            let err = require_grant(&params_with_grant(Some(grant))).unwrap_err();
            assert_eq!(
                message_of(&err),
                UNSUPPORTED_GRANT_TYPE,
                "`{grant}` is named as unsupported"
            );
        }
    }

    #[test]
    fn the_password_grant_passes_the_gate() {
        assert_eq!(
            require_grant(&params_with_grant(Some(GRANT_TYPE_PASSWORD))).unwrap(),
            Grant::Password,
            "password selects the user-bound grant"
        );
    }

    #[test]
    fn the_client_credentials_grant_passes_the_gate() {
        assert_eq!(
            require_grant(&params_with_grant(Some(GRANT_TYPE_CLIENT_CREDENTIALS))).unwrap(),
            Grant::ClientCredentials,
            "client_credentials selects the app-only grant"
        );
    }

    #[test]
    fn the_refresh_token_grant_passes_the_gate() {
        assert_eq!(
            require_grant(&params_with_grant(Some(GRANT_TYPE_REFRESH_TOKEN))).unwrap(),
            Grant::RefreshToken,
            "refresh_token selects the rotation grant"
        );
    }

    #[test]
    fn the_authorization_code_grant_passes_the_gate() {
        assert_eq!(
            require_grant(&params_with_grant(Some(GRANT_TYPE_AUTHORIZATION_CODE))).unwrap(),
            Grant::AuthorizationCode,
            "authorization_code selects the code-redemption grant"
        );
    }

    #[test]
    fn basic_credentials_reads_the_pair_from_the_header() {
        let encoded = STANDARD.encode("client-id:client-secret");
        for scheme in ["Basic", "basic", "BASIC"] {
            assert_eq!(
                basic_credentials(&headers_with(&format!("{scheme} {encoded}"))),
                Some(("client-id".to_owned(), "client-secret".to_owned())),
                "`{scheme}` is the basic scheme"
            );
        }
    }

    #[test]
    fn basic_credentials_falls_back_on_anything_it_cannot_read() {
        // Wrong scheme, lone scheme, bad base64, non-UTF-8 bytes and a
        // payload without the `id:secret` colon are all no-pair cases
        // (Pleroma's with-block falls through to the body params).
        let cases = [
            String::from("Bearer tok3n"),
            String::from("Basic"),
            String::from("Basic not-valid-base64!!!"),
            format!("Basic {}", STANDARD.encode([0xff_u8, 0xfe, 0xfd])),
            format!("Basic {}", STANDARD.encode("just-one-field")),
        ];
        for value in &cases {
            assert_eq!(
                basic_credentials(&headers_with(value)),
                None,
                "`{value:?}` carries no usable pair"
            );
        }
    }

    #[test]
    fn body_credentials_need_both_fields() {
        let mut params = params_with_grant(Some(GRANT_TYPE_PASSWORD));
        assert_eq!(params.body_credentials(), None, "nothing sent");
        params.client_id = Some("id".to_owned());
        assert_eq!(params.body_credentials(), None, "an id without a secret");
        params.client_secret = Some("secret".to_owned());
        assert_eq!(
            params.body_credentials(),
            Some(("id", "secret")),
            "the pair completes"
        );
    }

    #[test]
    fn secret_matches_compares_without_leaking_a_prefix() {
        assert!(secret_matches("client-secret", "client-secret"), "equal");
        assert!(!secret_matches("client-secret", "client-secre"), "shorter");
        assert!(
            !secret_matches("client-secret", "client-sEcRet"),
            "same length, different bytes"
        );
        assert!(!secret_matches("a", "ab"), "a prefix is not a match");
    }

    #[test]
    fn requested_scopes_default_to_read() {
        for blank in [None, Some(""), Some("   ")] {
            let mut params = params_with_grant(Some(GRANT_TYPE_PASSWORD));
            params.scope = blank.map(str::to_owned);
            let scopes = requested_scopes(&params, &app_row("read")).expect("read is a subset");
            assert_eq!(
                scopes.to_string(),
                Scopes::default().to_string(),
                "`{blank:?}` falls back to the default"
            );
        }
    }

    #[test]
    fn requested_scopes_must_fit_inside_the_registration() {
        let mut params = params_with_grant(Some(GRANT_TYPE_PASSWORD));
        params.scope = Some("read:accounts write:statuses".to_owned());
        let scopes = requested_scopes(&params, &app_row("read write")).expect("sub-scopes fit");
        assert_eq!(
            scopes.to_string(),
            "read:accounts write:statuses",
            "the requested subset is granted verbatim"
        );

        params.scope = Some("read write".to_owned());
        let err =
            requested_scopes(&params, &app_row("read")).expect_err("write was not registered");
        assert_eq!(
            message_of(&err),
            INVALID_SCOPE,
            "a broad request is refused"
        );
    }

    #[test]
    fn params_debug_redacts_the_secret_fields() {
        let mut params = params_with_grant(Some(GRANT_TYPE_PASSWORD));
        params.username = Some("alice".to_owned());
        params.password = Some("correct horse battery staple".to_owned());
        params.client_id = Some("client-id".to_owned());
        params.client_secret = Some("client-secret".to_owned());
        params.refresh_token = Some("refresh-secret".to_owned());
        params.code = Some("authorization-code".to_owned());
        let debug = format!("{params:?}");
        assert!(
            !debug.contains("correct horse battery staple")
                && !debug.contains("client-secret")
                && !debug.contains("refresh-secret")
                && !debug.contains("authorization-code"),
            "the secrets must not leak: {debug}"
        );
        assert!(
            debug.contains(r#"password: "<redacted>""#)
                && debug.contains(r#"client_secret: "<redacted>""#)
                && debug.contains(r#"refresh_token: "<redacted>""#)
                && debug.contains(r#"code: "<redacted>""#),
            "the markers name their fields: {debug}"
        );
        assert!(
            debug.contains("alice") && debug.contains("client-id"),
            "the rest shows"
        );
    }

    #[test]
    fn json_and_form_shapes_deserialize_to_the_same_params() {
        let full: TokenParams = crate::extract::into_params(json!({
            "grant_type": "refresh_token",
            "username": "alice",
            "password": "pw",
            "scope": "read write",
            "client_id": "id",
            "client_secret": "secret",
            "refresh_token": "a-refresh-token",
        }))
        .unwrap();
        assert_eq!(
            full.grant_type.as_deref(),
            Some(GRANT_TYPE_REFRESH_TOKEN),
            "the grant"
        );
        assert_eq!(full.username.as_deref(), Some("alice"), "the username");
        assert_eq!(
            full.scope.as_deref(),
            Some("read write"),
            "the scope string"
        );
        assert_eq!(full.client_id.as_deref(), Some("id"), "the client id");
        assert_eq!(
            full.refresh_token.as_deref(),
            Some("a-refresh-token"),
            "the refresh token"
        );
        let empty: TokenParams = crate::extract::into_params(json!({})).unwrap();
        assert!(
            empty.grant_type.is_none() && empty.username.is_none(),
            "an empty body (or a form without the keys) leaves everything absent"
        );
    }
}
