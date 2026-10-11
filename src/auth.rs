//! Bearer authentication: the `FromRequestParts<AppState>` extractors the
//! API builds on (`docs/ARCHITECTURE.md` decision 9).
//!
//! Where Pleroma runs these as plugs (`lib/pleroma/web/plugs/o_auth_plug.ex`
//! fetches the token, `ensure_authenticated_plug.ex` demands a user), here
//! it is one module of axum extractors. Every extractor reads the token
//! from the `Authorization: Bearer <token>` header — the scheme is matched
//! case-insensitively — falling back to the `access_token` query parameter,
//! which Pleroma and Mastodon accept for `GET` and the streaming
//! `WebSocket`; when both are present the header wins. A token is valid
//! when [`get_with_app`](crate::models::oauth_token::get_with_app) finds
//! its row, `valid_until` lies in the future (Pleroma's `Token.expired?`),
//! and — for user-bound tokens — the owner is still active.
//!
//! The extractors here and their rejections (all bodies are the
//! `{"error": …}` shape of decision 8, worded as Pleroma's OAuth fallback
//! renders them):
//!
//! - [`AppAuth`]: requires a valid token, app-only or user-bound alike
//!   (Pleroma's `OAuthPlug` assigns `token` and `app`;
//!   `GET /api/v1/apps/verify_credentials` is the endpoint that asks for
//!   this rather than a user). A missing, unknown, expired or
//!   deactivated-owner token is the 401 "The access token is invalid".
//!
//! The user-requiring extractors (`AuthUser`, `MaybeAuthUser`) arrive with
//! the endpoints that need them. Scope checks follow the same rule and live
//! beside those extractors.

use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, Uri, header::AUTHORIZATION, request::Parts},
};
use chrono::Utc;
use url::form_urlencoded;

use crate::{
    error::{AppError, AppResult},
    models::{
        oauth_app::OauthApp,
        oauth_token::{self, OauthToken},
        user,
    },
    state::AppState,
};

/// 401 body when no bearer token was presented, or the presented one does
/// not resolve to a live credential (Pleroma's OAuth fallback wording, as
/// `AppController.verify_credentials` sees it through `OAuthPlug`).
const INVALID_TOKEN: &str = "The access token is invalid";
/// `Authorization` scheme the header token is expected under, matched
/// case-insensitively.
const BEARER_SCHEME: &str = "Bearer";
/// Query parameter holding the bearer token (Pleroma's `OAuthPlug` accepts
/// it for `GET` and the streaming `WebSocket`).
const ACCESS_TOKEN_PARAM: &str = "access_token";

/// Extractor for endpoints that any valid token satisfies — app-only or
/// user-bound alike, e.g. `GET /api/v1/apps/verify_credentials`:
/// `AppAuth(app, token)`.
///
/// Rejection is the 401 "The access token is invalid" for a missing,
/// unknown, expired token or one whose owner's account is deactivated.
#[derive(Debug)]
pub struct AppAuth(
    /// The application the presented token was issued for.
    pub OauthApp,
    /// The valid token.
    pub OauthToken,
);

/// `AppAuth` from a valid token (app-only or user-bound); see
/// [`AppAuth`]'s docs for the rejections.
impl FromRequestParts<AppState> for AppAuth {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some(raw) = bearer_string(parts) else {
            return Err(AppError::Unauthorized(INVALID_TOKEN.to_owned()));
        };
        match resolve(state, &raw).await? {
            Auth::Valid(found) => Ok(Self(found.app, found.token)),
            Auth::Invalid => Err(AppError::Unauthorized(INVALID_TOKEN.to_owned())),
        }
    }
}

/// What looking a bearer string up in the database came to.
#[derive(Debug)]
enum Auth {
    /// The token exists, is unexpired and belongs to no deactivated
    /// account; the box holds the token row and its issuing application
    /// (boxed because that pair dwarfs [`Auth::Invalid`], and Clippy's
    /// `variant_size_differences` is denied).
    Valid(Box<TokenWithApp>),
    /// No row carries this token, the row is past `valid_until`, or the
    /// bound user's account is deactivated. [`AppAuth`] — which accepts
    /// any valid token — cannot tell these apart for its caller, so they
    /// share one 401.
    Invalid,
}

/// A resolved bearer credential: the token row and the application that
/// issued it.
#[derive(Debug)]
struct TokenWithApp {
    /// The `oauth_tokens` row the request presented.
    token: OauthToken,
    /// The `oauth_apps` row that issued it.
    app: OauthApp,
}

/// Looks the access token `raw` up with its app and rules it valid: the
/// row must exist, `valid_until` must lie in the future (Pleroma's
/// `Token.expired?`), and a bound user must still be active.
async fn resolve(state: &AppState, raw: &str) -> AppResult<Auth> {
    let Some((token, app)) = oauth_token::get_with_app(&state.db, raw).await? else {
        return Ok(Auth::Invalid);
    };
    if token.valid_until <= Utc::now() {
        return Ok(Auth::Invalid);
    }
    if let Some(user_id) = token.user_id {
        let owner = user::get_by_id(&state.db, user_id).await?;
        if owner.is_none_or(|found| !found.is_active.get()) {
            return Ok(Auth::Invalid);
        }
    }
    Ok(Auth::Valid(Box::new(TokenWithApp { token, app })))
}

/// The raw bearer token string the request presents: the
/// `Authorization` header first ("header wins"), then the
/// `access_token` query parameter.
fn bearer_string(parts: &Parts) -> Option<String> {
    bearer_from_headers(&parts.headers).or_else(|| bearer_from_query(&parts.uri))
}

/// The token of an `Authorization: Bearer <token>` header, the scheme
/// matched case-insensitively and any whitespace around the token
/// dropped; `None` for an absent or unparsable header, a different
/// scheme, or an empty token.
fn bearer_from_headers(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case(BEARER_SCHEME) {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

/// The `access_token` query parameter of `uri`, percent-decoded; the
/// first occurrence wins and an empty value counts as no token.
fn bearer_from_query(uri: &Uri) -> Option<String> {
    let query = uri.query()?;
    form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| *key == ACCESS_TOKEN_PARAM)
        .map(|(_, value)| value.into_owned())
        .filter(|token| !token.is_empty())
}

#[cfg(test)]
mod tests {
    use axum::{
        http::{Request, StatusCode, Uri, header::AUTHORIZATION},
        response::IntoResponse as _,
    };
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::models::{
        oauth_app::{self, NewOauthApp},
        oauth_token::NewToken,
    };
    use std::time::Duration;

    /// An `authorization: value` [`HeaderMap`].
    fn headers_with(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let previous: Option<axum::http::HeaderValue> =
            headers.insert(AUTHORIZATION, value.parse().unwrap());
        assert!(previous.is_none(), "a fresh map holds no header");
        headers
    }

    /// `target` as a [`Uri`].
    fn parse_uri(target: &str) -> Uri {
        target.parse().unwrap()
    }

    #[test]
    fn bearer_from_headers_matches_the_scheme_case_insensitively() {
        for scheme in ["Bearer", "bearer", "BEARER"] {
            let headers = headers_with(&format!("{scheme} tok3n"));
            assert_eq!(
                bearer_from_headers(&headers).as_deref(),
                Some("tok3n"),
                "`{scheme}` is the bearer scheme"
            );
        }
    }

    #[test]
    fn bearer_from_headers_needs_a_token_after_the_scheme() {
        for value in [
            "Basic dXNlcjpwYXNz",
            "Token abc123",
            "Bearer",
            "Bearer   ",
            "",
        ] {
            assert_eq!(
                bearer_from_headers(&headers_with(value)),
                None,
                "`{value:?}` carries no bearer token"
            );
        }
        assert_eq!(
            bearer_from_headers(&HeaderMap::new()),
            None,
            "no header, no token"
        );
    }

    #[test]
    fn bearer_from_headers_trims_the_token_but_keeps_its_body() {
        assert_eq!(
            bearer_from_headers(&headers_with("Bearer \ttab-token \t")).as_deref(),
            Some("tab-token"),
            "surrounding whitespace is dropped"
        );
        assert_eq!(
            bearer_from_headers(&headers_with("Bearer first second")).as_deref(),
            Some("first second"),
            "the rest of the value is the token, Pleroma's regex aside"
        );
    }

    #[test]
    fn bearer_from_query_reads_the_access_token_parameter() {
        assert_eq!(
            bearer_from_query(&parse_uri("/x?access_token=q4ry")).as_deref(),
            Some("q4ry"),
            "a lone parameter"
        );
        assert_eq!(
            bearer_from_query(&parse_uri("/x?a=1&access_token=q4ry&b=2")).as_deref(),
            Some("q4ry"),
            "the parameter among others"
        );
        assert_eq!(
            bearer_from_query(&parse_uri("/x?access_token=q%2B4ry")).as_deref(),
            Some("q+4ry"),
            "the value is percent-decoded"
        );
        for target in ["/x", "/x?other=1", "/x?access_token="] {
            assert_eq!(
                bearer_from_query(&parse_uri(target)),
                None,
                "`{target}` carries no token"
            );
        }
    }

    #[test]
    fn the_header_wins_over_the_query_parameter() {
        let (parts, ()) = Request::get("/x?access_token=from-query")
            .header(AUTHORIZATION, "Bearer from-header")
            .body(())
            .unwrap()
            .into_parts();
        let found = bearer_string(&parts).expect("one of the two is there");
        assert_eq!(found, "from-header", "header first, Pleroma's order");
    }

    /// Inserts a bare local user and returns its id.
    async fn insert_user(pool: &PgPool, nickname: &str) -> Uuid {
        let id = Uuid::now_v7();
        let outcome =
            sqlx::query("INSERT INTO users (id, nickname, ap_id, local) VALUES ($1, $2, $3, true)")
                .bind(id)
                .bind(nickname)
                .bind(format!("https://test.example/users/{nickname}"))
                .execute(pool)
                .await
                .unwrap();
        assert_eq!(
            outcome.rows_affected(),
            1,
            "the fixture insert wrote the row"
        );
        id
    }

    /// Registers a test app and mints a token issued by it.
    async fn mint_token(pool: &PgPool, user_id: Option<Uuid>, ttl: Duration) -> String {
        let app = oauth_app::create(
            pool,
            &NewOauthApp {
                client_name: "Auth Test".to_owned(),
                redirect_uris: "https://client.example/cb".to_owned(),
                scopes: "read".to_owned(),
                website: None,
                user_id: None,
            },
        )
        .await
        .unwrap();
        oauth_token::create_token(
            pool,
            &NewToken {
                user_id,
                app_id: app.id,
                scopes: "read".to_owned(),
                ttl: Some(ttl),
            },
        )
        .await
        .unwrap()
        .token
    }

    /// Runs the [`AppAuth`] extractor for a request carrying `token` in
    /// the `Authorization` header (`None` sends no header at all).
    async fn authenticate(pool: PgPool, token: Option<&str>) -> AppResult<AppAuth> {
        let mut request = Request::get("/api/v1/apps/verify_credentials");
        if let Some(raw) = token {
            request = request.header(AUTHORIZATION, format!("Bearer {raw}"));
        }
        let (mut parts, ()) = request.body(()).unwrap().into_parts();
        let state = AppState { db: pool.clone() };
        AppAuth::from_request_parts(&mut parts, &state).await
    }

    /// The status and body of a rejection.
    async fn render(err: AppError) -> (StatusCode, String) {
        let response = err.into_response();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap_or_default()
            .to_vec();
        (status, String::from_utf8(body).unwrap_or_default())
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_valid_app_only_token_authenticates(pool: PgPool) {
        let raw = mint_token(&pool, None, Duration::from_secs(600)).await;
        let AppAuth(app, token) = authenticate(pool, Some(&raw))
            .await
            .expect("the app-only token is valid");
        assert_eq!(app.client_name, "Auth Test", "the issuing app");
        assert_eq!(token.token, raw, "the presented token");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_valid_user_bound_token_authenticates(pool: PgPool) {
        let user_id = insert_user(&pool, "authtoken").await;
        let raw = mint_token(&pool, Some(user_id), Duration::from_secs(600)).await;
        let AppAuth(app, token) = authenticate(pool, Some(&raw))
            .await
            .expect("a user token satisfies AppAuth too");
        assert_eq!(app.client_name, "Auth Test", "the issuing app");
        assert_eq!(token.user_id, Some(user_id), "the bound user");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn no_token_is_the_invalid_token_401(pool: PgPool) {
        let err = authenticate(pool, None).await.expect_err("no credentials");
        let (status, body) = render(err).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "a 401");
        assert_eq!(body, r#"{"error":"The access token is invalid"}"#, "body");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn unknown_expired_and_deactivated_tokens_are_the_same_401(pool: PgPool) {
        let user_id = insert_user(&pool, "deadaccount").await;
        let lapsed = mint_token(&pool, Some(user_id), Duration::from_secs(1)).await;
        let live = mint_token(&pool, Some(user_id), Duration::from_secs(600)).await;
        let outcome = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(outcome.rows_affected(), 1, "the update hit the user row");
        for raw in ["never-minted", &lapsed, &live] {
            let err = authenticate(pool.clone(), Some(raw))
                .await
                .expect_err("an unknown, lapsed or dead-owner token must not authenticate");
            let (status, body) = render(err).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "a 401 for {raw}");
            assert_eq!(
                body, r#"{"error":"The access token is invalid"}"#,
                "the Pleroma wording for {raw}"
            );
        }
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn an_access_token_query_parameter_authenticates(pool: PgPool) {
        let raw = mint_token(&pool, None, Duration::from_secs(600)).await;
        let (mut parts, ()) = Request::get(format!("/x?access_token={raw}"))
            .body(())
            .unwrap()
            .into_parts();
        let state = AppState { db: pool };
        let AppAuth(app, _) = AppAuth::from_request_parts(&mut parts, &state)
            .await
            .expect("the query token authenticates");
        assert_eq!(app.client_name, "Auth Test", "the issuing app");
    }
}
