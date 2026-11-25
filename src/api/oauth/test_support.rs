//! Fixtures shared by the `oauth` children's unit tests: the parameter
//! builder and message reader the grant-gate and refusal tests use, the
//! stored-looking token row the response shapes render. The endpoint tests
//! build their state on [`lazy_state`](crate::test_support::lazy_state):
//! its lazy pool is never consulted, because every case fails at a gate
//! before any database access.

use chrono::{Duration, Utc};
use uuid::Uuid;

use super::params::{ClientCredentials, TokenParams};
use crate::{error::AppError, models::oauth_token::OauthToken, util::id::new_id};

/// A [`TokenParams`] with everything absent but `grant_type`, which
/// `grant` sets.
pub(super) fn params_with_grant(grant: Option<&str>) -> TokenParams {
    TokenParams {
        grant_type: grant.map(str::to_owned),
        username: None,
        password: None,
        scope: None,
        client: ClientCredentials::default(),
        refresh_token: None,
        code: None,
        redirect_uri: None,
        code_verifier: None,
    }
}

/// The message an [`AppError`] answers with.
pub(super) fn message_of(err: &AppError) -> String {
    err.to_string()
}

/// A stored-looking token row for `app_id`, valid for an hour.
pub(super) fn token_row(app_id: Uuid, scopes: &str) -> OauthToken {
    let now = Utc::now();
    OauthToken {
        id: new_id(),
        token: "access-secret".to_owned(),
        refresh_token: "refresh-secret".to_owned(),
        user_id: Some(new_id()),
        app_id,
        scopes: scopes.to_owned(),
        valid_until: now + Duration::hours(1),
        inserted_at: now,
        updated_at: now,
    }
}
