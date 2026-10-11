//! Announcements over HTTP: the notices this instance posts for every account,
//! and one account's dismissal of one of them.
//!
//! The reference is Pleroma's `AnnouncementController.index` and `.mark_read`
//! (`lib/pleroma/web/mastodon_api/controllers/announcement_controller.ex`).
//!
//! `GET /api/v1/announcements` answers the notices whose activity window holds
//! this instant ([`active_for_user`](crate::models::announcement::active_for_user)),
//! newest first, each rendered as the Mastodon `Announcement` entity extended
//! with Pleroma's `pleroma.raw_content`
//! ([`crate::entities::announcement`]). Every account sees the same notices
//! with a different `read` flag: `with_dismissed` (default `false`) decides
//! whether the ones the caller dismissed are left out of the list or shown
//! with `read: true`.
//!
//! `POST /api/v1/announcements/{id}/dismiss` records that dismissal for the
//! caller — nothing else changes, and nobody else's list is affected — and
//! answers the empty object. An id that is not `UUID` text and an id that names
//! no notice are both the 404 "Record not found" Pleroma's fallback renders,
//! and dismissing twice is the same 200: the second call writes no marker and
//! reports the same `{}`.
//!
//! Both routes require a user token ([`AuthUser`]) and, per the ticket's scope
//! pair, `read:accounts` to list and `write:accounts` to dismiss. That is
//! stricter than Pleroma's pipeline on `index`, which accepts any scope at all
//! as long as a user is behind the token; the ticket's rule wins here.

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Map, Value};
use tracing::debug;

use crate::{
    auth::AuthUser,
    entities::announcement::Announcement,
    error::{AppError, AppResult},
    extract::ApiParams,
    models::announcement,
    oauth::scopes::{READ_ACCOUNTS, WRITE_ACCOUNTS},
    state::AppState,
    util::id::parse_id,
};

/// The `/api/v1/announcements` router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/announcements", get(list_announcements))
        .route("/announcements/{id}/dismiss", post(dismiss_announcement))
}

/// The query parameters of `GET /api/v1/announcements`.
#[derive(Debug, Deserialize)]
struct ListParams {
    /// Whether notices the caller dismissed are listed too, with `read` set.
    /// Absent means `false`; a form client's `"true"`, `"1"` and the rest
    /// spell the same thing through [`flex::opt_bool`](crate::extract::flex).
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    with_dismissed: Option<bool>,
}

/// `GET /api/v1/announcements` — the notices active right now for the
/// caller, newest first, dismissed ones included only when `with_dismissed`
/// asks for them.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `read:accounts` check; [`AppError::Unprocessable`] when
/// `with_dismissed` is neither a boolean nor a spelling of one. Any database
/// failure arrives through `?` as [`AppError::Database`].
async fn list_announcements(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiParams(params): ApiParams<ListParams>,
) -> AppResult<Json<Vec<Announcement>>> {
    auth.require(READ_ACCOUNTS)?;
    let active = announcement::active_for_user(
        &state.db,
        auth.0.id,
        params.with_dismissed.unwrap_or_default(),
    )
    .await?;
    Ok(Json(
        active
            .iter()
            .map(|row| Announcement::from_row(&row.announcement, row.read))
            .collect(),
    ))
}

/// `POST /api/v1/announcements/{id}/dismiss` — mark the notice dismissed for
/// the caller and answer `{}`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:accounts` check; [`AppError::NotFound`] for an id that is
/// malformed or names no notice (Pleroma's `get_by_id` miss). Any database
/// failure arrives through `?` as [`AppError::Database`].
async fn dismiss_announcement(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    let Some(notice_id) = parse_id(&id) else {
        return Err(AppError::NotFound);
    };
    // The existence check is what turns an unknown notice into the 404 the
    // endpoint owes; without it the foreign key would fail the insert instead.
    if announcement::get(&state.db, notice_id).await?.is_none() {
        return Err(AppError::NotFound);
    }
    let marked = announcement::mark_read(&state.db, notice_id, auth.0.id).await?;
    debug!(
        %marked,
        user = %auth.0.id,
        announcement = %notice_id,
        "announcement dismissal recorded"
    );
    Ok(Json(Value::Object(Map::new())))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Whether [`ListParams`] reads `value` as asking for dismissed notices:
    /// the absent case is `false`, the spellings a client sends are `true`.
    fn dismissed_wanted(value: Value) -> bool {
        let params: ListParams = serde_json::from_value(value).expect("the fixture deserializes");
        params.with_dismissed.unwrap_or_default()
    }

    #[test]
    fn an_absent_parameter_means_dismissed_notices_are_hidden() {
        assert!(
            !dismissed_wanted(json!({})),
            "no key at all, the default list"
        );
        assert!(
            !dismissed_wanted(json!({"with_dismissed": null})),
            "an explicit null is the key present with no value"
        );
        assert!(
            !dismissed_wanted(json!({"with_dismissed": false})),
            "and a real boolean false"
        );
    }

    #[test]
    fn every_boolean_spelling_of_true_is_accepted() {
        for value in [json!(true), json!("true"), json!("1"), json!("yes")] {
            assert!(
                dismissed_wanted(json!({"with_dismissed": value})),
                "`{value}` asks for the dismissed ones"
            );
        }
    }

    #[test]
    fn every_boolean_spelling_of_false_is_accepted() {
        for value in [json!(false), json!("false"), json!("0"), json!("no")] {
            assert!(
                !dismissed_wanted(json!({"with_dismissed": value})),
                "`{value}` asks for the plain list"
            );
        }
    }

    #[test]
    fn a_value_that_is_not_a_boolean_is_rejected() {
        for value in [json!("maybe"), json!(2), json!([])] {
            let err = serde_json::from_value::<ListParams>(json!({"with_dismissed": value}))
                .expect_err("a parameter that is not a boolean is rejected");
            assert!(
                err.to_string().contains("expected a boolean"),
                "the message says what was wanted: {err}"
            );
        }
    }
}
