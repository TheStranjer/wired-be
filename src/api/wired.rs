//! `GET /api/v1/wired/protocols`: the account's identity on each protocol.
//!
//! A Wired extension (`docs/ARCHITECTURE.md` decision 17), advertised by the
//! `wired:protocols` feature flag. It is core: it works with `ActivityPub`
//! disabled, because it asks every enabled protocol, not `ActivityPub`
//! alone. `WiredFE` reads it to offer "Would you like to get on `<protocol>`?".
//!
//! The answer is one [`ProtocolEntry`] per enabled protocol, in registry
//! order:
//!
//! ```json
//! {"protocol": "activitypub", "status": "active", "identifier": "bob",
//!  "uri": "https://example.com/users/bob", "suggestion": null}
//! {"protocol": "fixture", "status": "unclaimed", "identifier": null,
//!  "uri": null, "suggestion": "bob"}
//! ```
//!
//! - `status` is `active` or `pending` as the adapter's
//!   [`identity`](ProtocolAdapter::identity) reports it, and `unclaimed` when
//!   the account is not on the protocol.
//! - `identifier` and `uri` are the identity's, and `null` while `unclaimed`.
//! - `suggestion` is set only while `unclaimed`: the ID
//!   [`suggest_identifier`] finds free for the account's nickname.
//!
//! The route needs a user token with the `read:accounts` scope, like
//! `verify_credentials`.
//!
//! # Claiming an identity
//!
//! `POST /api/v1/wired/protocols/{protocol}/claim` gets an account onto a
//! protocol it is not on yet: the late enrolment of an account that
//! registered before the operator enabled the protocol. The body is
//! `{"identifier": "optional"}`. A blank or missing `identifier` takes the
//! ID [`suggest_identifier`] finds free; a given one is checked by the
//! protocol's [`validate_identifier`](ProtocolAdapter::validate_identifier).
//! The claim is [`claim_identity`], the operation registration also uses for
//! `wired_identifiers`, so both paths judge an ID the same way.
//!
//! - 200 with the [`ProtocolEntry`] `GET /api/v1/wired/protocols` gives for
//!   that protocol, now `active`;
//! - 404 for a protocol id that is unknown or not enabled;
//! - 422 for an account already on the protocol, an invalid ID or a taken
//!   one, in the field-error shape of a failed registration, under the
//!   protocol's [`identifier_field`](ProtocolAdapter::identifier_field).
//!
//! It needs a user token with the `write:accounts` scope.

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::{
    auth::AuthUser,
    config::Config,
    error::{AppError, AppResult},
    extract::ApiBody,
    models::user::{User, register::claim_identity},
    oauth::scopes::{READ_ACCOUNTS, WRITE_ACCOUNTS},
    protocols::{
        adapter::ProtocolAdapter,
        identity::{IdentityStatus, suggest_identifier},
        registry,
    },
    state::AppState,
};

/// Whether the account is on a protocol, as the client sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryStatus {
    /// The identity is claimed and in use.
    Active,
    /// The identity exists but is awaiting setup.
    Pending,
    /// The account has no identity on the protocol.
    Unclaimed,
}

impl From<IdentityStatus> for EntryStatus {
    /// The identity's status, under the name the client sees.
    fn from(status: IdentityStatus) -> Self {
        match status {
            IdentityStatus::Active => Self::Active,
            IdentityStatus::Pending => Self::Pending,
        }
    }
}

/// The account's standing on one protocol: one element of the answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProtocolEntry {
    /// The protocol's id string, [`ProtocolId::as_str`](crate::protocols::ProtocolId::as_str).
    pub protocol: &'static str,
    /// Whether the account is on the protocol.
    pub status: EntryStatus,
    /// The account's ID on the protocol; `None` while unclaimed.
    pub identifier: Option<String>,
    /// The ID as a URI; `None` while unclaimed.
    pub uri: Option<String>,
    /// A free ID to claim; set only while unclaimed.
    pub suggestion: Option<String>,
}

/// The body of `POST /api/v1/wired/protocols/{protocol}/claim`.
#[derive(Debug, Deserialize)]
struct ClaimParams {
    /// The ID the account asks for; blank or missing asks for the suggestion.
    #[serde(default)]
    identifier: Option<String>,
}

/// The `/api/v1/wired` router, nested into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/protocols", get(protocols))
        .route("/protocols/{protocol}/claim", post(claim))
}

/// `GET /api/v1/wired/protocols` — the caller's identity on each enabled protocol.
///
/// # Errors
///
/// [`Unauthorized`](crate::error::AppError::Unauthorized) /
/// [`Forbidden`](crate::error::AppError::Forbidden) from [`AuthUser`] and the
/// `read:accounts` check; [`Conflict`](crate::error::AppError::Conflict) when
/// an unclaimed protocol has no free ID to suggest; any database failure.
async fn protocols(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<Vec<ProtocolEntry>>> {
    auth.require(READ_ACCOUNTS)?;
    let mut entries = Vec::new();
    for adapter in registry::enabled(&state.config) {
        entries.push(entry_for(adapter, &state, &auth.0).await?);
    }
    Ok(Json(entries))
}

/// `POST /api/v1/wired/protocols/{protocol}/claim` — gets the caller onto `protocol`.
///
/// # Errors
///
/// [`Unauthorized`](AppError::Unauthorized) /
/// [`Forbidden`](AppError::Forbidden) from [`AuthUser`] and the `write:accounts`
/// check; [`NotFound`](AppError::NotFound) for a protocol that is not enabled;
/// [`Unprocessable`](AppError::Unprocessable) from [`claim_identity`] when the
/// account is already on the protocol or the ID is invalid or taken; any
/// database failure.
async fn claim(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(protocol): Path<String>,
    ApiBody(params): ApiBody<ClaimParams>,
) -> AppResult<Json<ProtocolEntry>> {
    auth.require(WRITE_ACCOUNTS)?;
    let adapter = enabled_adapter(&state.config, &protocol).ok_or(AppError::NotFound)?;
    claim_identity(&state, adapter, &auth.0, params.identifier.as_deref()).await?;
    Ok(Json(entry_for(adapter, &state, &auth.0).await?))
}

/// The adapter of the enabled protocol whose id string is `protocol`, if there is one.
fn enabled_adapter(config: &Config, protocol: &str) -> Option<&'static dyn ProtocolAdapter> {
    registry::enabled(config).find(|adapter| adapter.id().as_str() == protocol)
}

/// `user`'s standing on `adapter`'s protocol.
///
/// # Errors
///
/// Any database failure, or [`Conflict`](crate::error::AppError::Conflict)
/// when an unclaimed protocol has no free ID to suggest.
async fn entry_for(
    adapter: &dyn ProtocolAdapter,
    state: &AppState,
    user: &User,
) -> AppResult<ProtocolEntry> {
    let protocol = adapter.id().as_str();
    if let Some(identity) = adapter.identity(state, user).await? {
        return Ok(ProtocolEntry {
            protocol,
            status: identity.status.into(),
            identifier: Some(identity.identifier),
            uri: Some(identity.uri),
            suggestion: None,
        });
    }
    let mut conn = state.db.acquire().await?;
    let suggestion = suggest_identifier(adapter, &mut conn, &user.nickname, &state.config).await?;
    Ok(ProtocolEntry {
        protocol,
        status: EntryStatus::Unclaimed,
        identifier: None,
        uri: None,
        suggestion: Some(suggestion),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The statuses serialise as the lowercase words the client reads.
    #[test]
    fn statuses_serialise_in_lowercase() {
        let json = |status| serde_json::to_value(status).expect("a status serialises");
        assert_eq!(json(EntryStatus::Active), "active");
        assert_eq!(json(EntryStatus::Pending), "pending");
        assert_eq!(json(EntryStatus::Unclaimed), "unclaimed");
    }

    /// An identity's status maps onto the entry status of the same name.
    #[test]
    fn identity_statuses_map_to_entry_statuses() {
        assert_eq!(
            EntryStatus::from(IdentityStatus::Active),
            EntryStatus::Active
        );
        assert_eq!(
            EntryStatus::from(IdentityStatus::Pending),
            EntryStatus::Pending
        );
    }
}
