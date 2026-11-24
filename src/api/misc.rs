//! `MongooseIM` auth endpoints and the small unauthenticated utility
//! endpoints (`docs/ARCHITECTURE.md` decision 1, WBE-166).
//!
//! Three mount points, one module each side of the shared handlers:
//!
//! - [`router`], merged by [`crate::app`]: the `MongooseIM` auth endpoints
//!   `GET /user_exists` and `GET /check_password` (Pleroma's
//!   `lib/pleroma/web/mongoose_im/mongoose_im_controller.ex`, served at
//!   the root for an `extauth` HTTP backend).
//! - [`util_router`], nested at `/pleroma` by [`crate::api::v1`]:
//!   `GET /api/v1/pleroma/captcha` and `GET /api/v1/pleroma/healthcheck`
//!   (Pleroma's `lib/pleroma/web/pleroma_api/controllers/util_controller.ex`).
//! - [`frontend_configurations_router`], merged by [`crate::api::pleroma`]:
//!   `GET /api/pleroma/frontend_configurations`, the same
//!   `UtilController.frontend_configurations/2`, reading its document from
//!   [`instance_setting`].
//!
//! The three toggles gate the first four endpoints: with
//! `MONGOOSEIM_ENABLED` off both auth endpoints are absent, and with
//! `CAPTCHA_ENABLED` / `HEALTHCHECK_ENABLED` off so are the captcha and
//! healthcheck answers — each disabled case is the app-wide fallback's
//! 404 `{"error":"Not found"}` ([`AppError::RouteNotFound`]), not Pleroma's
//! behavior of routing the path to some fallback. Where the ticket's shape
//! differs from Pleroma's controller the ticket wins, and the difference is
//! documented at the handler: `check_password` answers *every* failed
//! credential (unknown user included, where Pleroma splits 403 and 404)
//! with 403 `false`, and the disabled healthcheck answers 404 `Not found`
//! (Pleroma: 503 "Healthcheck disabled").
//!
//! None of the endpoints authenticates: `MongooseIM` is a trusted internal
//! caller (its controller runs on the `:api` pipeline, unauthenticated),
//! and the utility endpoints serve public data.

use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse as _, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use tracing::warn;

use crate::{
    error::{AppError, AppResult},
    models::{
        instance_setting,
        user::{User, get_by_nickname},
    },
    state::AppState,
    util::password::verify_password,
};

/// Seconds the healthcheck's `SELECT 1` probe may take before the database
/// counts as unhealthy (the ticket's one-second bound).
const PROBE_TIMEOUT_SECS: u64 = 1;
/// The only captcha type this instance can answer: no captcha
/// implementation exists, so [`captcha`] reports `"none"` like Pleroma's
/// `Pleroma.Captcha.new/0` with no provider configured.
const CAPTCHA_TYPE_NONE: &str = "none";
/// Body message of the 501 [`captcha`] answers when a provider is
/// configured but not implemented.
const CAPTCHA_NOT_IMPLEMENTED: &str = "Captcha is not implemented";
/// The instance setting whose document
/// [`frontend_configurations`] serves (Pleroma's `:frontend_configurations`
/// runtime config key).
const FRONTEND_CONFIGURATIONS_KEY: &str = "frontend_configurations";

/// The root-level router, merged by [`crate::app`]: the two `MongooseIM`
/// auth endpoints.
///
/// Both live outside `/api`, at addresses fixed by the `MongooseIM`
/// `extauth` http contract.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/user_exists", get(user_exists))
        .route("/check_password", get(check_password))
}

/// The `/api/v1/pleroma` utility router ([`crate::api::v1`] nests it at
/// `/pleroma`): Pleroma's `UtilController` endpoints this ticket serves.
pub fn util_router() -> Router<AppState> {
    Router::new()
        .route("/captcha", get(captcha))
        .route("/healthcheck", get(healthcheck))
}

/// The `/api/pleroma` frontend-configurations router, merged by
/// [`crate::api::pleroma`].
pub fn frontend_configurations_router() -> Router<AppState> {
    Router::new().route("/frontend_configurations", get(frontend_configurations))
}

/// The query of `GET /user_exists`: `MongooseIM` sends the handle under
/// `user`. It is optional here because its absence is the 404 `false`
/// answer, which the type cannot express.
#[derive(Debug, Deserialize)]
struct UserQuery {
    /// The nickname to look up.
    user: Option<String>,
}

/// The query of `GET /check_password`: handle under `user`, credential
/// under `pass` (`MongooseIM`'s `extauth` http parameter names). Both are
/// optional: a request missing either is the failed-credential answer.
#[derive(Debug, Deserialize)]
struct PasswordQuery {
    /// The nickname to authenticate.
    user: Option<String>,
    /// The password to verify against the stored hash.
    pass: Option<String>,
}

/// `GET /user_exists?user=<nick>` — the `MongooseIM` lookup: 200 with JSON
/// `true` when `user` names a local, active account, 404 with JSON `false`
/// for everything else (unknown nickname, remote mirror, deactivated
/// account, missing parameter). Disabled by
/// [`mongooseim_enabled`](crate::config::Config::mongooseim_enabled): then the endpoint
/// is the standard fallback 404.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] when disabled; [`AppError::Database`] on a
/// lookup failure. A failed *credential* is a `false` body, not an error.
async fn user_exists(
    State(state): State<AppState>,
    Query(query): Query<UserQuery>,
) -> AppResult<Response> {
    require_mongooseim(&state)?;
    let exists = match query.user.as_deref() {
        Some(nickname) => local_active_user_exists(&state.db, nickname).await?,
        None => false,
    };
    Ok(json_bool(exists, StatusCode::NOT_FOUND))
}

/// `GET /check_password?user=<nick>&pass=<password>` — the `MongooseIM`
/// credential check: 200 with JSON `true` when the password verifies for
/// the local, *active* user named by `user`, 403 with JSON `false`
/// otherwise (wrong password, unknown nickname, deactivated account,
/// missing parameter). Disabled by
/// [`mongooseim_enabled`](crate::config::Config::mongooseim_enabled): then the endpoint
/// is the standard fallback 404.
///
/// Unlike Pleroma's controller — which answers an unknown user 404 and a
/// wrong password 403 — the ticket fails every bad credential with 403,
/// so `MongooseIM` sees one uniform denial.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] when disabled; [`AppError::Database`] on a
/// lookup failure.
async fn check_password(
    State(state): State<AppState>,
    Query(query): Query<PasswordQuery>,
) -> AppResult<Response> {
    require_mongooseim(&state)?;
    let valid = match (query.user.as_deref(), query.pass.as_deref()) {
        (Some(nickname), Some(password)) => {
            valid_credentials(&state.db, nickname, password).await?
        }
        _ => false,
    };
    Ok(json_bool(valid, StatusCode::FORBIDDEN))
}

/// `GET /api/v1/pleroma/captcha` — the captcha handshake (Pleroma's
/// `UtilController.captcha/2`, which renders `Pleroma.Captcha.new/0`).
/// No captcha is implemented here: unless
/// [`captcha_enabled`](crate::config::Config::captcha_enabled) says a provider is
/// configured, the answer is `{"type":"none"}`; when one is, this
/// instance cannot serve it and answers 501 with the standard error
/// shape.
async fn captcha(State(state): State<AppState>) -> Response {
    if state.config.captcha_enabled.get() {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(CaptchaErrorBody {
                error: CAPTCHA_NOT_IMPLEMENTED,
            }),
        )
            .into_response();
    }
    Json(CaptchaConfig {
        captcha_type: CAPTCHA_TYPE_NONE,
    })
    .into_response()
}

/// `GET /api/v1/pleroma/healthcheck` — Pleroma's `UtilController.healthcheck/2`:
/// the pool's connection counts and a `SELECT 1` liveness probe, 200 when
/// healthy and 503 when not. Disabled by
/// [`healthcheck_enabled`](crate::config::Config::healthcheck_enabled): Pleroma answers
/// 503 `"Healthcheck disabled"` then; the ticket makes it the standard
/// fallback 404 instead, and the endpoint never fails on its own — a dead
/// database is the 503 body's `healthy: false`.
async fn healthcheck(State(state): State<AppState>) -> Response {
    if !state.config.healthcheck_enabled.get() {
        return AppError::RouteNotFound.into_response();
    }
    let healthy = probe_database(&state.db).await;
    let status = if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(HealthReport::measure(&state.db, healthy))).into_response()
}

/// `GET /api/pleroma/frontend_configurations` — the frontend configuration
/// object (Pleroma renders its `:frontend_configurations` runtime config
/// here). This instance keeps that document in the `instance_settings`
/// table; no row is the empty object. Any database failure is the usual
/// 500.
///
/// # Errors
///
/// [`AppError::Database`] when the lookup fails.
async fn frontend_configurations(State(state): State<AppState>) -> AppResult<Json<Value>> {
    let configured = instance_setting::get(&state.db, FRONTEND_CONFIGURATIONS_KEY).await?;
    Ok(Json(configured.unwrap_or_else(|| json!({}))))
}

/// The `MongooseIM` gate: unless [`mongooseim_enabled`](crate::config::Config::mongooseim_enabled)
/// is set, the auth endpoints do not exist — the standard fallback 404,
/// which is what an unconfigured instance answers an unknown path with.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] when the endpoints are disabled.
fn require_mongooseim(state: &AppState) -> AppResult<()> {
    if state.config.mongooseim_enabled.get() {
        Ok(())
    } else {
        Err(AppError::RouteNotFound)
    }
}

/// Whether `nickname` names a local, active user: exactly Pleroma's
/// `Repo.get_by(User, nickname: username, local: true, is_active: true)`,
/// with the citext comparison [`get_by_nickname`] runs.
///
/// # Errors
///
/// [`AppError::Database`] when the lookup fails.
async fn local_active_user_exists(pool: &PgPool, nickname: &str) -> AppResult<bool> {
    Ok(get_by_nickname(pool, nickname)
        .await?
        .as_ref()
        .is_some_and(User::is_local_active))
}

/// Whether `password` is the credential of the local, active user named
/// `nickname`: the row is looked up by *nickname* only — `MongooseIM`
/// authenticates handles, not emails, so unlike the login path this never
/// matches on `email` — must be local and active, must carry a hash, and
/// [`verify_password`] must accept that hash.
///
/// # Errors
///
/// [`AppError::Database`] when the lookup fails.
async fn valid_credentials(pool: &PgPool, nickname: &str, password: &str) -> AppResult<bool> {
    let Some(found) = get_by_nickname(pool, nickname).await? else {
        return Ok(false);
    };
    if !found.is_local_active() {
        return Ok(false);
    }
    let Some(stored) = found.password_hash.as_deref() else {
        return Ok(false);
    };
    Ok(verify_password(password, stored))
}

/// The `MongooseIM` answer: JSON `true` under 200, JSON `false` under
/// `false_status` — 404 for [`user_exists`], 403 for [`check_password`],
/// the codes the ticket fixes for each endpoint's negative case.
fn json_bool(value: bool, false_status: StatusCode) -> Response {
    let status = if value { StatusCode::OK } else { false_status };
    (status, Json(value)).into_response()
}

/// Whether the database answers `SELECT 1` within
/// [`PROBE_TIMEOUT_SECS`]. Every failure — a closed pool, a timeout, any
/// database error — is unhealthy; the detail is logged and the client
/// only ever sees the flag.
async fn probe_database(pool: &PgPool) -> bool {
    let probe = sqlx::query("SELECT 1").execute(pool);
    match tokio::time::timeout(Duration::from_secs(PROBE_TIMEOUT_SECS), probe).await {
        Ok(Ok(_rows)) => true,
        Ok(Err(err)) => {
            warn!(%err, "healthcheck probe failed");
            false
        }
        Err(_elapsed) => {
            warn!("healthcheck probe timed out");
            false
        }
    }
}

/// The captcha answer document: Pleroma's `Pleroma.Captcha.new/0` returns
/// the configured provider's settings, and `%{"type" => "none"}` with no
/// provider — the only state this instance has.
#[derive(Debug, Serialize)]
struct CaptchaConfig {
    /// The provider type: [`CAPTCHA_TYPE_NONE`].
    #[serde(rename = "type")]
    captcha_type: &'static str,
}

/// The 501 body [`captcha`] answers when [`captcha_enabled`](crate::config::Config::captcha_enabled)
/// claims a provider: the standard error shape (`{"error": ...}`) with the
/// ticket's message, spelled out here because [`AppError`] has no 501
/// variant and this is not a server failure to route through it.
#[derive(Debug, Serialize)]
struct CaptchaErrorBody {
    /// The client-safe message: [`CAPTCHA_NOT_IMPLEMENTED`].
    error: &'static str,
}

/// The healthcheck answer body, the five keys of Pleroma's
/// `Pleroma.Healthcheck` struct minus `job_queue_stats` (this instance
/// has no job-queue metrics to report and the ticket's body lists these
/// five only). Field declaration order is the ticket's body order.
#[derive(Debug, Serialize)]
struct HealthReport {
    /// Connections the pool currently holds, in use and idle
    /// (`sqlx::Pool::size`).
    pool_size: u32,
    /// Connections checked out of the pool: [`pool_size`](Self::pool_size)
    /// minus [`idle`](Self::idle).
    active: u32,
    /// Connections the pool holds that no task is using
    /// (`sqlx::Pool::num_idle`).
    idle: u32,
    /// Memory used, in megabytes: Pleroma reports its BEAM allocation
    /// here; nothing measures this process, so the ticket's `0.0` for
    /// "unavailable".
    memory_used: f64,
    /// Whether the `SELECT 1` probe succeeded within the time bound.
    healthy: bool,
}

impl HealthReport {
    /// Reads the pool's current counts and pairs them with the probe's
    /// `healthy` verdict.
    fn measure(pool: &PgPool, healthy: bool) -> Self {
        let idle = u32::try_from(pool.num_idle()).unwrap_or(u32::MAX);
        let pool_size = pool.size();
        Self {
            pool_size,
            active: pool_size.saturating_sub(idle),
            idle,
            memory_used: 0.0,
            healthy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The captcha answer is the single-key document Pleroma's provider-
    /// less `Captcha.new/0` renders.
    #[test]
    fn captcha_config_is_type_none() {
        let body = serde_json::to_string(&CaptchaConfig {
            captcha_type: CAPTCHA_TYPE_NONE,
        })
        .unwrap();
        assert_eq!(body, r#"{"type":"none"}"#, "the none answer");
    }

    /// The 501 body keeps the standard error shape and the ticket's
    /// message.
    #[test]
    fn captcha_error_body_is_the_standard_shape() {
        let body = serde_json::to_string(&CaptchaErrorBody {
            error: CAPTCHA_NOT_IMPLEMENTED,
        })
        .unwrap();
        assert_eq!(
            body, r#"{"error":"Captcha is not implemented"}"#,
            "the 501 body"
        );
    }

    /// The healthcheck body carries the five keys in the ticket's order,
    /// with `memory_used` at the unavailable `0.0`.
    #[test]
    fn health_report_is_the_ticket_body() {
        let body = serde_json::to_string(&HealthReport {
            pool_size: 10,
            active: 2,
            idle: 8,
            memory_used: 0.0,
            healthy: true,
        })
        .unwrap();
        assert_eq!(
            body, r#"{"pool_size":10,"active":2,"idle":8,"memory_used":0.0,"healthy":true}"#,
            "the exact healthcheck body, in order"
        );
    }

    /// [`json_bool`] pairs the ticket's two bodies with the two status
    /// shapes each endpoint owes for a `false` answer.
    #[tokio::test]
    async fn json_bool_pairs_the_answer_with_its_negative_status() {
        use http_body_util::BodyExt as _;

        for (value, status, body) in [
            (true, StatusCode::OK, "true"),
            (false, StatusCode::NOT_FOUND, "false"),
            (false, StatusCode::FORBIDDEN, "false"),
        ] {
            let response = json_bool(value, status);
            assert_eq!(response.status(), status, "status of {body}");
            assert_eq!(
                response.headers().get("content-type").unwrap(),
                "application/json",
                "the answer is JSON"
            );
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(bytes, body, "body of {value}");
        }
    }

    /// The pool counts come from `size()` and `num_idle()`, with `active`
    /// what a lazy pool holds back from the idle side; a lazy pool that
    /// never connected reports all-zero counts.
    #[tokio::test]
    async fn health_report_measures_the_pool_counts() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(7)
            .connect_lazy(crate::config::TEST_DATABASE_URL)
            .unwrap();
        let report = HealthReport::measure(&pool, true);
        assert_eq!(report.pool_size, 0, "a lazy pool holds no connection yet");
        assert_eq!(report.idle, 0, "and so none idle");
        assert_eq!(report.active, 0, "and none checked out either");
        assert_eq!(report.memory_used, 0.0, "memory is not measured");
        assert!(report.healthy, "the verdict is the caller's");
    }
}
