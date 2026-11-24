//! The OAuth MFA challenge flow (`/oauth/mfa*`, WBE-132): the second
//! factor that stands between a correct password and a token.
//!
//! When an MFA-enabled account logs in through the `password` grant, no
//! token is handed out (the external-login callback refuses such an
//! account outright). Instead the grant's parameters are parked on a pending
//! [`oauth_token::create_authorization`] row, a five-minute
//! [`mfa_token::create`] challenge links to it, and the client answers it
//! at `POST /oauth/mfa/challenge` (Pleroma's
//! `lib/pleroma/web/o_auth/mfa_controller.ex`). There is no server-rendered
//! code-entry page: the frontend prompts in-app.
//!
//! `POST /oauth/mfa/challenge` — client authentication
//! exactly like the other token calls (body pair or HTTP Basic), plus
//! `mfa_token`, `challenge_type` (`totp` or `recovery`) and `code`. A
//! passed challenge retires the challenge row, spends the parked
//! authorization and answers the very token JSON the password grant
//! withheld; every other answer is the 400 `{"error":"Invalid code"}`
//! — Pleroma does not tell a wrong code, an unknown token, an expired
//! token and a foreign app apart here.
//!
//! The second factor itself is [`crate::util::totp`]: `totp` verifies
//! the authenticator's six-digit code inside the ±1-step window, and
//! `recovery` matches the code's SHA-256 against the stored backup-code
//! hashes, removing a hit so each code works exactly once.
//!
//! A wrong code is counted against its challenge
//! ([`mfa_token::record_failure`]); the challenge that has taken
//! [`MAX_MFA_ATTEMPTS`] of them is deleted, so the client must restart
//! from the password, and the route also runs in the limiter's
//! `authentication` bucket (WBE-668).
//!
//! Every answer — token or error — carries
//! `Cache-Control: no-store` like the rest of `/oauth`: challenge tokens
//! and codes are single-use material.

use std::fmt;

use axum::{
    Json, Router,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse as _, Response},
    routing::post,
};
use chrono::Utc;
use serde::Deserialize;

use super::{
    TokenResponse, issue_token, no_cache_headers, params::ClientCredentials, validate_app,
};
use crate::{
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{
        mfa_token::{self, MAX_MFA_ATTEMPTS, MfaToken},
        oauth_app::OauthApp,
        oauth_token::{self, OauthAuthorization},
        user::{self, User, mfa},
    },
    oauth::scopes::Scopes,
    state::AppState,
    util::totp,
};

/// The challenge type of the authenticator's six-digit code.
const CHALLENGE_TYPE_TOTP: &str = "totp";
/// The challenge type of a printed backup (recovery) code.
const CHALLENGE_TYPE_RECOVERY: &str = "recovery";
/// The one 400 body every failed challenge shares (Pleroma's
/// `MFAController.challenge` else-branch): a wrong code, an unknown or
/// expired token and a foreign application answer alike.
const INVALID_CODE: &str = "Invalid code";

/// Whether `user`'s account stands behind the second factor: the
/// `enabled` switch of the settings column (Pleroma's `MFA.require?`).
pub(crate) fn required(user: &User) -> bool {
    mfa::MfaSettings::for_user(user).enabled
}

/// Starts an MFA challenge for `user`: the grant's parameters are parked
/// on a fresh pending authorization (its scopes answer the challenge; no
/// code is ever delivered from it, so it binds no redirect URI), and a
/// five-minute [`MfaToken`] row links it (Pleroma's
/// `MFA.Token.create(user, auth)`). Returns the challenge token text.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
pub(crate) async fn start_challenge(
    state: &AppState,
    user: &User,
    app: &OauthApp,
    scopes: &Scopes,
) -> AppResult<String> {
    let authorization =
        oauth_token::create_authorization(&state.db, user.id, app.id, &scopes.to_string(), "")
            .await?;
    let challenge = mfa_token::create(&state.db, user.id, Some(authorization.id)).await?;
    Ok(challenge.token)
}

/// The `/oauth/mfa/challenge` route, merged into the `/oauth` router by
/// [`super::router`].
///
/// It checks a code, so it runs in the limiter's `authentication` bucket
/// (WBE-668) and `state` is handed over for the middleware layer.
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new().route(
        "/oauth/mfa/challenge",
        post(challenge).layer(crate::rate_limit::rate_limit(state, "authentication")),
    )
}

/// Counts a wrong code against the challenge `stored` (WBE-668).
///
/// Once it has taken [`MAX_MFA_ATTEMPTS`] of them the row is deleted, so
/// the client must restart from the password.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
async fn note_failure(state: &AppState, stored: &MfaToken) -> AppResult<()> {
    let attempts = mfa_token::record_failure(&state.db, stored.id).await?;
    if attempts >= MAX_MFA_ATTEMPTS {
        mfa_token::delete(&state.db, stored.id).await?;
    }
    Ok(())
}

/// The one 400 every challenge failure shares.
fn invalid_code() -> AppError {
    AppError::BadRequest(INVALID_CODE.to_owned())
}

/// `POST /oauth/mfa/challenge` — the API half of the second factor.
/// Every answer, success or error, gets the token endpoint's no-store
/// headers.
async fn challenge(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiBody(params): ApiBody<ChallengeParams>,
) -> Response {
    let response = match run_challenge(&state, &headers, &params).await {
        Ok(body) => body.into_response(),
        Err(err) => err.into_response(),
    };
    no_cache_headers(response)
}

/// The challenge in Pleroma's order: the application, the still-valid
/// challenge row, its account, the parked authorization behind it, and
/// the code — then the withheld token pair. Client authentication runs
/// the same gate as on `/oauth/token`, but here even its failure answers
/// [`INVALID_CODE`], because the challenge refuses as one voice.
async fn run_challenge(
    state: &AppState,
    headers: &HeaderMap,
    params: &ChallengeParams,
) -> AppResult<Json<TokenResponse>> {
    let app = match validate_app(state, headers, params.client.pair()).await {
        Ok(app) => app,
        // Credential refusals answer alike; a database failure stays one.
        Err(AppError::BadRequest(_)) => return Err(invalid_code()),
        Err(err) => return Err(err),
    };
    let stored = valid_challenge(state, params.mfa_token.as_deref())
        .await?
        .ok_or_else(invalid_code)?;
    let user = user::get_by_id(&state.db, stored.user_id)
        .await?
        .ok_or_else(invalid_code)?;
    let authorization = pending_authorization(state, &stored)
        .await?
        .ok_or_else(invalid_code)?;
    if !second_factor_ok(
        state,
        &user,
        params.challenge_type(),
        params.code.as_deref(),
    )
    .await?
    {
        note_failure(state, &stored).await?;
        return Err(invalid_code());
    }
    finish_challenge(state, &user, &app, &stored, &authorization).await
}

/// The answer to a passed challenge: the parked authorization is spent
/// first — its never-delivered code must not also be redeemable, and
/// only by the client that started the grant — then the challenge row
/// retires, and the token pair the grant withheld is minted from it
/// (Pleroma's `Token.exchange_token(app, auth)`).
async fn finish_challenge(
    state: &AppState,
    user: &User,
    app: &OauthApp,
    stored: &MfaToken,
    authorization: &OauthAuthorization,
) -> AppResult<Json<TokenResponse>> {
    if authorization.app_id != app.id {
        return Err(invalid_code());
    }
    mfa_token::delete(&state.db, stored.id).await?;
    let consumed = oauth_token::consume_authorization(
        &state.db,
        &authorization.token,
        app.id,
        authorization.redirect_uri.as_deref(),
    )
    .await?;
    if consumed.is_none() {
        return Err(invalid_code());
    }
    let scopes = Scopes::parse(&authorization.scopes);
    let token = issue_token(state, Some(user), app, scopes).await?;
    Ok(Json(TokenResponse::new(token, Some(user))))
}

/// The stored challenge `presented` names, while it is still within its
/// five minutes; an absent or blank token finds nothing, and unknown and
/// expired are the same `None` (Pleroma's `MFA.Token.validate`).
async fn valid_challenge(state: &AppState, presented: Option<&str>) -> AppResult<Option<MfaToken>> {
    let Some(token) = presented.filter(|text| !text.is_empty()) else {
        return Ok(None);
    };
    mfa_token::get_valid(&state.db, token).await
}

/// The pending authorization the challenge links: the grant parameters
/// that outlive the login. A missing link, a gone row, one whose account
/// is not the challenged one, or one past its ten minutes is `None`,
/// which the endpoints refuse like a wrong code.
async fn pending_authorization(
    state: &AppState,
    stored: &MfaToken,
) -> AppResult<Option<OauthAuthorization>> {
    let Some(id) = stored.authorization_id else {
        return Ok(None);
    };
    let found = oauth_token::get_authorization_by_id(&state.db, id).await?;
    let matching =
        found.filter(|auth| auth.user_id == stored.user_id && auth.valid_until > Utc::now());
    Ok(matching)
}

/// Checks the submitted code against the account's second factor:
/// `totp` is the authenticator's code inside the ±1-step window
/// [`totp::verify`] allows, `recovery` a printed backup code spent on
/// use; a missing or unknown challenge type, and a missing or blank
/// code, never pass.
///
/// # Errors
///
/// Consuming a backup code writes the account's settings; any database
/// failure arrives as [`AppError::Database`].
async fn second_factor_ok(
    state: &AppState,
    user: &User,
    challenge_type: Option<&str>,
    code: Option<&str>,
) -> AppResult<bool> {
    match (challenge_type, code.filter(|text| !text.is_empty())) {
        (Some(CHALLENGE_TYPE_TOTP), Some(text)) => Ok(totp_matches(user, text)),
        (Some(CHALLENGE_TYPE_RECOVERY), Some(text)) => consume_backup_code(state, user, text).await,
        _ => Ok(false),
    }
}

/// Whether `code` is the authenticator's current code (or one step's
/// clock drift off) under the account's TOTP secret; an account without
/// a secret never matches.
fn totp_matches(user: &User, code: &str) -> bool {
    let settings = mfa::MfaSettings::for_user(user);
    settings
        .totp
        .secret
        .as_deref()
        .is_some_and(|secret| totp::verify(secret, code, Utc::now().timestamp()))
}

/// Spends a backup code: when the SHA-256 of `code` matches one of the
/// stored hashes, that hash comes off the list — the code works exactly
/// once, even across concurrent challenges, because [`mfa::consume_backup_code`]
/// removes it in a single atomic statement.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
async fn consume_backup_code(state: &AppState, user: &User, code: &str) -> AppResult<bool> {
    mfa::consume_backup_code(&state.db, user.id, &totp::hash_backup_code(code)).await
}

/// The request parameters of `POST /oauth/mfa/challenge`. Every field is
/// optional at the shape level — the endpoint answers the missing ones
/// with its own `Invalid code`, so the handler checks each field, as the
/// token endpoint's parameters do. The manual `Debug` impl (see below)
/// redacts the three secret fields.
#[derive(Deserialize)]
struct ChallengeParams {
    /// The `mfa_token` from the refused password grant. Renders as
    /// `<redacted>` in `Debug` output.
    #[serde(default)]
    mfa_token: Option<String>,
    /// `totp` or `recovery`; anything else never passes.
    #[serde(default)]
    challenge_type: Option<String>,
    /// The submitted code. Never in `Debug` output.
    #[serde(default)]
    code: Option<String>,
    /// The client credentials, when not presented through HTTP Basic.
    #[serde(flatten)]
    client: ClientCredentials,
}

/// [`mfa_token`](Self::mfa_token), [`code`](Self::code) and the
/// [`client`](Self::client) secret are secret material and render
/// as `<redacted>`; the challenge type and client id show.
impl fmt::Debug for ChallengeParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChallengeParams")
            .field("mfa_token", &"<redacted>")
            .field("challenge_type", &self.challenge_type)
            .field("code", &"<redacted>")
            .field("client", &self.client)
            .finish()
    }
}

impl ChallengeParams {
    /// The submitted challenge type, verbatim.
    fn challenge_type(&self) -> Option<&str> {
        self.challenge_type.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Request, StatusCode, header::CACHE_CONTROL};
    use serde_json::{Value, json};
    use tower::ServiceExt as _;

    use super::*;
    use crate::{api::oauth::NO_STORE, config::Config};

    /// A minimal stored-looking local user row.
    fn user_row() -> User {
        let now = Utc::now();
        User {
            id: uuid::Uuid::now_v7(),
            nickname: "alice".to_owned(),
            ap_id: "https://test.example/users/alice".to_owned(),
            local: user::Flag::new(true),
            domain: None,
            actor_type: "Person".to_owned(),
            email: None,
            password_hash: None,
            name: "alice".to_owned(),
            bio: String::new(),
            raw_bio: String::new(),
            avatar_url: None,
            banner_url: None,
            profile_url: None,
            locked: user::Flag::new(false),
            bot: user::Flag::new(false),
            discoverable: user::Flag::new(false),
            fields: json!([]),
            raw_fields: json!([]),
            inbox: None,
            shared_inbox: None,
            outbox: None,
            followers_url: None,
            following_url: None,
            featured_url: None,
            public_key: None,
            private_key: None,
            is_admin: user::Flag::new(false),
            is_moderator: user::Flag::new(false),
            is_active: user::Flag::new(true),
            hide_followers: user::Flag::new(false),
            hide_follows: user::Flag::new(false),
            hide_followers_count: user::Flag::new(false),
            hide_follows_count: user::Flag::new(false),
            hide_favorites: user::Flag::new(true),
            allow_following_move: user::Flag::new(true),
            skip_thread_containment: user::Flag::new(false),
            accepts_chat_messages: user::Flag::new(true),
            show_role: user::Flag::new(true),
            no_rich_text: user::Flag::new(false),
            default_scope: String::new(),
            sensitive_default: user::Flag::new(false),
            language: None,
            tags: Vec::new(),
            settings_store: json!({}),
            notification_settings: json!({}),
            mfa_settings: mfa::MfaSettings::default_value(),
            background_image_url: None,
            birthday: None,
            show_birthday: user::Flag::new(false),
            also_known_as: Vec::new(),
            moved_to_ap_id: None,
            last_move_at: None,
            email_confirmed: user::Flag::new(true),
            approval_pending: user::Flag::new(false),
            confirmation_token: None,
            confirmation_sent_at: None,
            registration_reason: None,
            last_active_at: None,
            followers_count: 0,
            following_count: 0,
            statuses_count: 0,
            last_status_at: None,
            last_fetched_at: None,
            inserted_at: now,
            updated_at: now,
            is_suggested: user::Flag::new(false),
            password_reset_pending: user::Flag::new(false),
            email_notifications: user::Flag::new(true),
            deleted_at: None,
        }
    }

    /// A [`user_row`] whose `mfa_settings` column holds `value` (the
    /// model's lenient parser reads it back at the gate).
    fn user_with_mfa(value: Value) -> User {
        let mut account = user_row();
        account.mfa_settings = value;
        account
    }

    #[test]
    fn the_gate_is_the_enabled_switch() {
        assert!(!required(&user_row()), "the all-off default is no gate");
        let unconfirmed = user_with_mfa(json!({
            "enabled": false,
            "totp": {"secret": "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ", "confirmed": false},
        }));
        assert!(
            !required(&unconfirmed),
            "an unconfirmed secret has not enabled MFA yet"
        );
        let gated = user_with_mfa(json!({
            "enabled": true,
            "totp": {"secret": "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ", "confirmed": true},
            "backup_codes": ["hash"],
        }));
        assert!(required(&gated), "an enabled account is gated");
    }

    #[test]
    fn totp_matches_only_the_secret_holders_current_code() {
        let secret = totp::generate_secret();
        let account = user_with_mfa(json!({
            "enabled": true,
            "totp": {"secret": secret, "confirmed": true},
        }));
        let now = Utc::now().timestamp();
        let code = totp::code_at(&secret, now).unwrap();
        assert!(totp_matches(&account, &code), "the current step passes");
        let (head, tail) = code.split_at(1);
        let typo = format!("{}{tail}", if head == "0" { "1" } else { "0" });
        assert!(!totp_matches(&account, &typo), "a wrong code does not pass");
        let no_secret = user_with_mfa(json!({"enabled": true}));
        assert!(
            !totp_matches(&no_secret, &code),
            "an account without a secret never matches"
        );
    }

    #[test]
    fn a_recovery_challenge_spends_the_matching_backup_code_once() {
        // The pure half of `consume_backup_code`: which stored hash a
        // submitted code hits; the database write and the exactly-once
        // consequence are covered over HTTP in tests/oauth_mfa.rs.
        let codes = totp::generate_backup_codes(2);
        let mut settings = mfa::MfaSettings::default();
        settings.rotate_backup_codes(
            codes
                .iter()
                .map(|code| totp::hash_backup_code(code))
                .collect(),
        );
        let stored = settings.backup_codes.clone();
        for code in &codes {
            let hash = totp::hash_backup_code(code);
            assert!(stored.contains(&hash), "an issued code matches its hash");
        }
        assert!(
            !stored.contains(&totp::hash_backup_code("zzzzzzzz")),
            "a code that was never issued matches nothing"
        );
    }

    #[test]
    fn challenge_params_debug_redacts_the_credentials() {
        let params: ChallengeParams = crate::extract::into_params(json!({
            "mfa_token": "challenge-token",
            "challenge_type": "totp",
            "code": "123456",
            "client_id": "client-id",
            "client_secret": "client-secret",
        }))
        .unwrap();
        let debug = format!("{params:?}");
        assert!(
            !debug.contains("challenge-token")
                && !debug.contains("123456")
                && !debug.contains("client-secret"),
            "the secrets must not leak: {debug}"
        );
        assert!(
            debug.contains(r#"mfa_token: "<redacted>""#)
                && debug.contains(r#"code: "<redacted>""#)
                && debug.contains(r#"client_secret: "<redacted>""#),
            "the markers name their fields: {debug}"
        );
        assert!(
            debug.contains("totp") && debug.contains("client-id"),
            "the rest shows"
        );
    }

    #[test]
    fn json_and_form_bodies_fill_the_challenge_credentials() {
        let none: ChallengeParams = crate::extract::into_params(json!({})).unwrap();
        assert_eq!(none.client.pair(), None, "nothing sent");
        let from_json: ChallengeParams = serde_json::from_str(
            r#"{"mfa_token":"t","code":"1","client_id":"id","client_secret":"s"}"#,
        )
        .unwrap();
        assert_eq!(from_json.client.pair(), Some(("id", "s")), "a JSON body");
        let from_form: ChallengeParams = crate::extract::into_params(json!({
            "mfa_token": "t",
            "code": "1",
            "client_id": "id",
            "client_secret": "s",
        }))
        .unwrap();
        assert_eq!(from_form.client.pair(), Some(("id", "s")), "a form body");
        assert_eq!(from_form.code.as_deref(), Some("1"), "the siblings fill");
    }

    /// The state the route-shaped tests below need: config only, the
    /// lazy pool is never consulted (every refusal these tests send
    /// fails before the first query).
    fn test_state() -> AppState {
        AppState::lazy_for_tests(Config::for_tests())
    }

    /// The MFA router over [`test_state`].
    fn test_router() -> Router {
        let state = test_state();
        router(&state).with_state(state)
    }

    #[tokio::test]
    async fn the_challenge_route_refuses_an_empty_body() {
        let app = test_router();
        let request = Request::post("/oauth/mfa/challenge")
            .header("content-type", "application/json")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "no credentials is the challenge's one refusal"
        );
        assert_eq!(
            response.headers().get(CACHE_CONTROL),
            Some(&axum::http::HeaderValue::from_static(NO_STORE)),
            "and the no-store headers"
        );
    }

    #[tokio::test]
    async fn the_retired_html_pages_are_not_routed() {
        for request in [
            Request::get("/oauth/mfa?mfa_token=t0ken&state=st")
                .body(axum::body::Body::empty())
                .unwrap(),
            Request::post("/oauth/mfa/verify")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json!({}).to_string()))
                .unwrap(),
        ] {
            let uri = request.uri().to_string();
            let response = test_router().oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{uri} is no longer a route"
            );
        }
    }
}
