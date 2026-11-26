//! The Mastodon 4.3+ notification policy and notification request routes.
//!
//! `GET`/`PATCH /api/v1/notifications/policy` take boolean `filter_*` fields,
//! `GET`/`PATCH /api/v2/notifications/policy` take `for_*` fields holding
//! `accept`, `filter` or `drop`, and `GET /api/v1/notifications/requests`
//! with `…/requests/merged` is the request list.
//!
//! This server has no notification filtering by sender class (the only
//! sender-based filter it has is the Pleroma `block_from_strangers` flag,
//! see [`crate::api::notification_settings`]), and so no held-back
//! notification requests. Rather than store a policy nothing would honour,
//! the read endpoints answer Mastodon's "accept everything" policy with an
//! empty summary and the request list is always empty, and a write is
//! refused with a 422 unless it only asks for what is already true (every
//! `filter_*` false, every `for_*` `accept`), which changes nothing and
//! answers the policy. Clients therefore see a consistent, honest policy
//! instead of a setting that silently does nothing.
//!
//! Reads require `read:notifications` and the writes `write:notifications`.

use axum::{Json, Router, routing::get};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    auth::AuthUser,
    error::{AppError, AppResult},
    extract::{ApiBody, flex},
    oauth::scopes::{READ_NOTIFICATIONS, WRITE_NOTIFICATIONS},
    state::AppState,
};

/// The only `for_*` value this server can honour: deliver the notification.
const ACCEPT: &str = "accept";
/// The other two `for_*` values Mastodon defines, accepted as spellings but
/// refused because nothing here can hold or drop a notification.
const KNOWN_ACTIONS: [&str; 3] = [ACCEPT, "filter", "drop"];

/// The `/api/v1` policy and request routes, merged into
/// [`crate::api::v1::core_router`].
///
/// Do not call [`Router::fallback`] here: unmatched paths must reach the
/// app-wide JSON fallback in [`crate::app`].
pub fn v1_router() -> Router<AppState> {
    Router::new()
        .route("/notifications/policy", get(show_v1).patch(update_v1))
        .route("/notifications/requests", get(list_requests))
        .route("/notifications/requests/merged", get(requests_merged))
}

/// The `/api/v2` policy route, merged into [`crate::api::v2::core_router`].
pub fn v2_router() -> Router<AppState> {
    Router::new().route("/notifications/policy", get(show_v2).patch(update_v2))
}

/// The body of `PATCH /api/v1/notifications/policy`: each flag optional.
#[derive(Debug, Deserialize)]
struct PolicyV1Params {
    /// `filter_not_following`: hold notifications from accounts not followed.
    #[serde(
        default,
        rename = "filter_not_following",
        deserialize_with = "flex::opt_bool"
    )]
    not_following: Option<bool>,
    /// `filter_not_followers`: hold notifications from non-followers.
    #[serde(
        default,
        rename = "filter_not_followers",
        deserialize_with = "flex::opt_bool"
    )]
    not_followers: Option<bool>,
    /// `filter_new_accounts`: hold notifications from accounts under 30 days.
    #[serde(
        default,
        rename = "filter_new_accounts",
        deserialize_with = "flex::opt_bool"
    )]
    new_accounts: Option<bool>,
    /// `filter_private_mentions`: hold unsolicited private mentions.
    #[serde(
        default,
        rename = "filter_private_mentions",
        deserialize_with = "flex::opt_bool"
    )]
    private_mentions: Option<bool>,
}

/// The body of `PATCH /api/v2/notifications/policy`: each action optional.
#[derive(Debug, Deserialize)]
struct PolicyV2Params {
    /// `for_not_following`: what to do with accounts not followed.
    #[serde(default, rename = "for_not_following")]
    not_following: Option<String>,
    /// `for_not_followers`: what to do with non-followers.
    #[serde(default, rename = "for_not_followers")]
    not_followers: Option<String>,
    /// `for_new_accounts`: what to do with accounts under 30 days old.
    #[serde(default, rename = "for_new_accounts")]
    new_accounts: Option<String>,
    /// `for_private_mentions`: what to do with unsolicited private mentions.
    #[serde(default, rename = "for_private_mentions")]
    private_mentions: Option<String>,
    /// `for_limited_accounts`: what to do with moderator-limited accounts.
    #[serde(default, rename = "for_limited_accounts")]
    limited_accounts: Option<String>,
}

/// The `summary` block of both policies: nothing is ever held back.
fn summary() -> Value {
    json!({ "pending_requests_count": 0, "pending_notifications_count": 0 })
}

/// The v1 policy this server always has: nothing is filtered.
fn policy_v1() -> Value {
    json!({
        "filter_not_following": false,
        "filter_not_followers": false,
        "filter_new_accounts": false,
        "filter_private_mentions": false,
        "summary": summary(),
    })
}

/// The v2 policy this server always has: everything is accepted.
fn policy_v2() -> Value {
    json!({
        "for_not_following": ACCEPT,
        "for_not_followers": ACCEPT,
        "for_new_accounts": ACCEPT,
        "for_private_mentions": ACCEPT,
        "for_limited_accounts": ACCEPT,
        "summary": summary(),
    })
}

/// The 422 for a policy change this server cannot honour.
fn unsupported(field: &str) -> AppError {
    AppError::Unprocessable(format!(
        "Notification filtering is not supported: `{field}` cannot be changed, \
         every notification is delivered"
    ))
}

/// `GET /api/v1/notifications/policy` — the accept-everything policy.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `read:notifications` check.
async fn show_v1(auth: AuthUser) -> AppResult<Json<Value>> {
    auth.require(READ_NOTIFICATIONS)?;
    Ok(Json(policy_v1()))
}

/// `PATCH /api/v1/notifications/policy` — answer the policy when the body
/// asks for no filtering, refuse anything else.
///
/// # Errors
///
/// The auth errors of [`show_v1`] with `write:notifications`;
/// [`AppError::Unprocessable`] for a flag set to `true` or a malformed one.
async fn update_v1(
    auth: AuthUser,
    ApiBody(params): ApiBody<PolicyV1Params>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_NOTIFICATIONS)?;
    let flags = [
        ("filter_not_following", params.not_following),
        ("filter_not_followers", params.not_followers),
        ("filter_new_accounts", params.new_accounts),
        ("filter_private_mentions", params.private_mentions),
    ];
    if let Some((name, _)) = flags.iter().find(|(_, value)| *value == Some(true)) {
        return Err(unsupported(name));
    }
    Ok(Json(policy_v1()))
}

/// `GET /api/v2/notifications/policy` — the accept-everything policy.
///
/// # Errors
///
/// As [`show_v1`].
async fn show_v2(auth: AuthUser) -> AppResult<Json<Value>> {
    auth.require(READ_NOTIFICATIONS)?;
    Ok(Json(policy_v2()))
}

/// `PATCH /api/v2/notifications/policy` — answer the policy when every
/// named action is `accept`, refuse anything else.
///
/// # Errors
///
/// As [`update_v1`]; a value outside `accept`, `filter` and `drop` is the
/// enum violation, also a 422.
async fn update_v2(
    auth: AuthUser,
    ApiBody(params): ApiBody<PolicyV2Params>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_NOTIFICATIONS)?;
    let actions = [
        ("for_not_following", &params.not_following),
        ("for_not_followers", &params.not_followers),
        ("for_new_accounts", &params.new_accounts),
        ("for_private_mentions", &params.private_mentions),
        ("for_limited_accounts", &params.limited_accounts),
    ];
    for (name, value) in actions {
        let Some(value) = value.as_deref() else {
            continue;
        };
        if !KNOWN_ACTIONS.contains(&value) {
            return Err(AppError::Unprocessable(format!(
                "Invalid parameters: {name} must be accept, filter or drop, got `{value}`"
            )));
        }
        if value != ACCEPT {
            return Err(unsupported(name));
        }
    }
    Ok(Json(policy_v2()))
}

/// `GET /api/v1/notifications/requests` — the held-back notification
/// requests: always none, as nothing is held back.
///
/// # Errors
///
/// As [`show_v1`].
async fn list_requests(auth: AuthUser) -> AppResult<Json<Vec<Value>>> {
    auth.require(READ_NOTIFICATIONS)?;
    Ok(Json(Vec::new()))
}

/// `GET /api/v1/notifications/requests/merged` — whether the background
/// merge of accepted requests is done: always, there being none.
///
/// # Errors
///
/// As [`show_v1`].
async fn requests_merged(auth: AuthUser) -> AppResult<Json<Value>> {
    auth.require(READ_NOTIFICATIONS)?;
    Ok(Json(json!({ "merged": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_v1_policy_filters_nothing() {
        let policy = policy_v1();
        for field in [
            "filter_not_following",
            "filter_not_followers",
            "filter_new_accounts",
            "filter_private_mentions",
        ] {
            assert_eq!(policy[field], json!(false), "{field}");
        }
        assert_eq!(policy["summary"]["pending_requests_count"], json!(0));
    }

    #[test]
    fn the_v2_policy_accepts_everything() {
        let policy = policy_v2();
        for field in [
            "for_not_following",
            "for_not_followers",
            "for_new_accounts",
            "for_private_mentions",
            "for_limited_accounts",
        ] {
            assert_eq!(policy[field], json!("accept"), "{field}");
        }
        assert_eq!(policy["summary"]["pending_notifications_count"], json!(0));
    }

    #[test]
    fn the_refusal_names_the_field() {
        let refusal = unsupported("for_new_accounts");
        assert!(
            matches!(&refusal, AppError::Unprocessable(message) if message.contains("for_new_accounts")),
            "{refusal:?}"
        );
    }
}
