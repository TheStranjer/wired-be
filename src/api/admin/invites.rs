//! The admin invite endpoints: the registration-invite maintenance API
//! under `/api/v1/pleroma/admin/users` (WBE-199).
//!
//! The reference is Pleroma's `AdminAPI.InviteController`
//! (`lib/pleroma/web/admin_api/controllers/invite_controller.ex`), routed
//! in `lib/pleroma/web/router.ex` and rendering through
//! `AdminAPI.InviteView` (`lib/pleroma/web/admin_api/views/invite_view.ex`).
//! Pleroma gates `index` with `admin:read:invites` and the three POSTs
//! with `admin:write:invites`; the same split lives here as the
//! [`AdminUser::require`] call each handler makes first. The invite
//! entity is the view's seven keys — `id`, `token`, `used`,
//! `expires_at` (the plain `YYYY-MM-DD` day or `null`), `uses`,
//! `max_use`, `invite_type` — rendered by
//! [`crate::entities::admin::AdminInvite`].
//!
//! - `POST /users/invite_token` (`create`) — mints one invite from the
//!   optional `max_use` and `expires_at` limits; their presence derives
//!   the type exactly like Pleroma's `assign_type` (see
//!   [`InviteType::for_params`](crate::models::invite_token::InviteType::for_params)),
//!   and the stored entity answers with 200. Records one `invite_create`
//!   [`moderation_log`] entry.
//! - `GET /users/invites` (`index`) — every invite as
//!   `{"invites": […]}`, newest first.
//! - `POST /users/revoke_invite` (`revoke`) — Pleroma's action marks the
//!   invite used; the ticket instead deletes the row and answers it, so a
//!   revoked invite leaves no spent row behind and the
//!   `invite_revoke` [`moderation_log`] entry is the surviving trace. An
//!   unknown `token` is 404 `{"error":"Not found"}`; a body without the
//!   parameter is 422.
//! - `POST /users/email_invite` (`email`) — while `INVITES_ENABLED` is
//!   off the endpoint refuses with Pleroma's 400; otherwise a one-time
//!   invite is minted and the [`user_invitation`](crate::mailer::templates::user_invitation)
//!   letter to the body's `email` is queued through
//!   [`mailer::deliver_later`], carrying the registration link
//!   `{FRONTEND_BASE_URL}/registration/{token}` — the frontend's own page;
//!   the backend serves no `/registration/{token}` route. The answer is 204 with an empty
//!   body; a body without `email` is the 400 the ticket pins.
//!   Pleroma reads the optional `name` as the recipient's display name;
//!   this mailer addresses bare recipients, so the parameter is accepted
//!   and goes no further than the debug line.
//!
//! [`moderation_log`]: crate::models::moderation_log
//! [`mailer::deliver_later`]: crate::mailer::deliver_later

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use chrono::NaiveDate;
use serde::Deserialize;
use tracing::debug;

use crate::{
    auth::admin::AdminUser,
    config::Config,
    entities::admin::{AdminInvite, InviteList},
    error::{AppError, AppResult},
    extract::{ApiBody, flex},
    mailer::{self, templates},
    models::{
        invite_token::{self, InviteParams},
        moderation_log,
    },
    state::AppState,
};

/// The read endpoint class of [`AdminUser::require`].
const CLASS_READ: &str = "read";
/// The write endpoint class of [`AdminUser::require`].
const CLASS_WRITE: &str = "write";
/// The admin scope area these endpoints live in
/// (`admin:read:invites` / `admin:write:invites`).
const AREA_INVITES: &str = "invites";
/// 400 body of `POST /users/email_invite` while `INVITES_ENABLED` is off
/// (Pleroma's `email` refusal, wording pinned by the ticket).
const INVITES_DISABLED: &str =
    "To send invites you need to set the `invites_enabled` option to true.";
/// 400 body of `POST /users/email_invite` with no `email` parameter.
const MISSING_EMAIL: &str = "Missing parameter: email";
/// 422 body of `POST /users/revoke_invite` with no `token` parameter
/// (Pleroma's `CastAndValidate` refusal wording).
const MISSING_TOKEN: &str = "Missing parameter: token";
/// The path prefix of the registration page the emailed letter links to on
/// the frontend.
const REGISTRATION_PREFIX: &str = "/registration/";

/// The invite routes of the admin API, merged into [`super`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/users/invite_token", post(create_invite))
        .route("/users/invites", get(list_invites))
        .route("/users/revoke_invite", post(revoke_invite))
        .route("/users/email_invite", post(email_invite))
}

/// The body of `POST /users/invite_token`: Pleroma's `create` parameters,
/// both optional — their presence derives the invite's type, and neither
/// is the one-time invite. `max_use` deserializes through
/// [`flex::opt_i32`] so a form-encoded request's numeric string counts.
#[derive(Debug, Default, Deserialize)]
struct CreateInviteBody {
    /// Registrations the invite may serve in total; `None` is uncapped.
    #[serde(default, deserialize_with = "flex::opt_i32")]
    max_use: Option<i32>,
    /// Last day the invite is valid; `None` never expires.
    #[serde(default)]
    expires_at: Option<NaiveDate>,
}

/// The body of `POST /users/revoke_invite`: Pleroma's `revoke`
/// parameters. `token` is required, but read as an `Option` so its
/// absence answers the 422 the ticket's validation rule gives rather
/// than the extractor's generic shape refusal.
#[derive(Debug, Deserialize)]
struct RevokeInviteBody {
    /// The token text of the invite to revoke.
    #[serde(default)]
    token: Option<String>,
}

/// The body of `POST /users/email_invite`: Pleroma's `email` parameters.
/// `email` is required and read as an `Option` for the same reason
/// [`RevokeInviteBody::token`] is; `name` — Pleroma's recipient display
/// name — is accepted and ignored, since this mailer addresses bare
/// recipients (see the module docs).
#[derive(Debug, Deserialize)]
struct EmailInviteBody {
    /// The address to send the invitation letter to.
    #[serde(default)]
    email: Option<String>,
    /// The recipient's display name; recorded only for the debug line.
    #[serde(default)]
    name: Option<String>,
}

/// `POST /api/v1/pleroma/admin/users/invite_token` — Pleroma's `create`:
/// mint one invite from the submitted limits, record the
/// `invite_create` log entry and answer the stored entity.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::Unprocessable`] when a limit
/// is no integer or no ISO date; [`AppError::Database`] from the insert
/// and the log write.
async fn create_invite(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiBody(body): ApiBody<CreateInviteBody>,
) -> AppResult<Json<AdminInvite>> {
    admin.require(CLASS_WRITE, AREA_INVITES)?;
    let params = InviteParams {
        expires_at: body.expires_at,
        max_use: body.max_use,
    };
    let invite = invite_token::create(&state.db, &params).await?;
    let logged = moderation_log::invite_create(&state.db, &admin.0, invite.id).await?;
    debug!(
        id = %logged.id,
        invite = %invite.id,
        invite_type = invite.invite_type.as_str(),
        "admin invite created"
    );
    Ok(Json(AdminInvite::from_invite(&invite)))
}

/// `GET /api/v1/pleroma/admin/users/invites` — Pleroma's `index`: every
/// invite as `{"invites": […]}`, newest first.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::Database`] from the listing
/// read.
async fn list_invites(
    State(state): State<AppState>,
    admin: AdminUser,
) -> AppResult<Json<InviteList>> {
    admin.require(CLASS_READ, AREA_INVITES)?;
    let invites = invite_token::list(&state.db).await?;
    Ok(Json(InviteList::newest_first(&invites)))
}

/// `POST /api/v1/pleroma/admin/users/revoke_invite` — Pleroma's `revoke`:
/// delete the invite its body's `token` names, record the
/// `invite_revoke` log entry and answer the entity as it stood. An
/// unknown token — like Pleroma's `find_by_token` miss — is the 404.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::Unprocessable`] when the body
/// carries no `token`; [`AppError::RouteNotFound`] for an unknown token;
/// [`AppError::Database`] from the delete and the log write.
async fn revoke_invite(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiBody(body): ApiBody<RevokeInviteBody>,
) -> AppResult<Json<AdminInvite>> {
    admin.require(CLASS_WRITE, AREA_INVITES)?;
    let Some(token) = body.token.as_deref() else {
        return Err(AppError::Unprocessable(MISSING_TOKEN.to_owned()));
    };
    let Some(invite) = invite_token::get_by_token(&state.db, token).await? else {
        return Err(AppError::RouteNotFound);
    };
    let removed = invite_token::revoke(&state.db, token).await?;
    let logged = moderation_log::invite_revoke(&state.db, &admin.0, invite.id).await?;
    debug!(
        id = %logged.id,
        invite = %invite.id,
        removed,
        "admin invite revoked"
    );
    Ok(Json(AdminInvite::from_invite(&invite)))
}

/// `POST /api/v1/pleroma/admin/users/email_invite` — Pleroma's `email`:
/// mint a one-time invite and queue the [`user_invitation`](crate::mailer::templates::user_invitation)
/// letter to the body's address, its link built on
/// [`Config::frontend_base_url`] (the frontend serves the registration
/// page). 204 with an empty body when the letter is on the queue.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] with
/// [`MISSING_EMAIL`] when the body names no address and with
/// [`INVITES_DISABLED`] while `INVITES_ENABLED` is off;
/// [`AppError::Database`] from the insert and the job enqueue.
async fn email_invite(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiBody(body): ApiBody<EmailInviteBody>,
) -> AppResult<StatusCode> {
    admin.require(CLASS_WRITE, AREA_INVITES)?;
    let address = body
        .email
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    let Some(to) = address else {
        return Err(AppError::BadRequest(MISSING_EMAIL.to_owned()));
    };
    if !state.config.invites_enabled.get() {
        return Err(AppError::BadRequest(INVITES_DISABLED.to_owned()));
    }
    let invite = invite_token::create(&state.db, &InviteParams::one_time()).await?;
    let logged = moderation_log::invite_create(&state.db, &admin.0, invite.id).await?;
    let link = registration_link(&state.config, &invite.token);
    let letter = templates::user_invitation(&state.config, to, &link);
    mailer::deliver_later(&state, letter).await?;
    debug!(
        id = %logged.id,
        invite = %invite.id,
        to = %to,
        named = body.name.is_some(),
        "admin email invite queued"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// The absolute registration link of `token` on `config`'s frontend
/// origin: [`Config::frontend_base_url`] (no trailing slash) followed by
/// [`REGISTRATION_PREFIX`] and the token.
fn registration_link(config: &Config, token: &str) -> String {
    format!("{}{REGISTRATION_PREFIX}{token}", config.frontend_base_url)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// `POST /users/invite_token` parses: absent limits, JSON numbers,
    /// the form request's numeric strings, and ISO dates — and an empty
    /// body is the one-time invite.
    #[test]
    fn the_create_body_reads_the_optional_limits() {
        let plain: CreateInviteBody =
            serde_json::from_value(json!({})).expect("an empty body deserializes");
        assert_eq!(plain.max_use, None, "no cap");
        assert_eq!(plain.expires_at, None, "no expiry");
        let defaults = CreateInviteBody::default();
        assert_eq!(
            (defaults.max_use, defaults.expires_at),
            (None, None),
            "and the default is the same"
        );

        let capped: CreateInviteBody =
            serde_json::from_value(json!({"max_use": 5, "expires_at": "2026-12-31"}))
                .expect("the ticket's shape deserializes");
        assert_eq!(capped.max_use, Some(5), "the JSON number");
        assert_eq!(
            capped.expires_at,
            Some(NaiveDate::from_ymd_opt(2026, 12, 31).expect("a valid day")),
            "and the ISO day"
        );

        let strings: CreateInviteBody = serde_json::from_value(json!({"max_use": "7"}))
            .expect("a numeric string reads through flex");
        assert_eq!(strings.max_use, Some(7), "the form spelling counts");

        let junk = serde_json::from_value::<CreateInviteBody>(json!({"max_use": "lots"}));
        assert!(junk.is_err(), "a non-number cap is no read");
    }

    /// The revoke body reads its `token` and reports its absence as
    /// `None` for the handler's 422.
    #[test]
    fn the_revoke_body_reads_the_token_or_its_absence() {
        let named: RevokeInviteBody =
            serde_json::from_value(json!({"token": "aBc-_123"})).expect("the ticket's shape");
        assert_eq!(named.token.as_deref(), Some("aBc-_123"), "the token text");
        let missing: RevokeInviteBody =
            serde_json::from_value(json!({})).expect("a body without it deserializes");
        assert_eq!(missing.token, None, "and reads as no token");
    }

    /// The email body reads `email` and the optional `name`, and the
    /// absence of either is the handler's to refuse.
    #[test]
    fn the_email_body_reads_address_and_name() {
        let full: EmailInviteBody =
            serde_json::from_value(json!({"email": "friend@example.org", "name": "Friend"}))
                .expect("the ticket's shape");
        assert_eq!(
            full.email.as_deref(),
            Some("friend@example.org"),
            "the address"
        );
        assert_eq!(full.name.as_deref(), Some("Friend"), "and the name");
        let addressless: EmailInviteBody =
            serde_json::from_value(json!({"name": "Friend"})).expect("a body without it");
        assert_eq!(addressless.email, None, "reads as no address");
    }

    /// The letter link is absolute on the frontend origin, never on the
    /// backend's own `base_url`.
    #[test]
    fn the_registration_link_is_absolute_on_the_frontend() {
        let config = Config::for_tests();
        assert_eq!(
            registration_link(&config, "abc"),
            format!("{}/registration/abc", config.frontend_base_url),
            "the default frontend origin"
        );

        let mut frontended = Config::for_tests();
        frontended.frontend_base_url = "https://fe.example".to_owned();
        assert_eq!(
            registration_link(&frontended, "xyz"),
            "https://fe.example/registration/xyz",
            "a separate frontend takes the link"
        );
        assert!(
            !registration_link(&frontended, "xyz").starts_with(frontended.base_url.as_str()),
            "and the backend origin does not"
        );
    }
}
