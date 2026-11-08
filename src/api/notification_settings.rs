//! Notification settings: `PUT /api/pleroma/notification_settings`
//! (WBE-111).
//!
//! The reference is Pleroma's `UtilController.update_notification_settings`
//! (`lib/pleroma/web/pleroma_api/controllers/util_controller.ex`) writing
//! the `Pleroma.User.NotificationSetting` embedded schema
//! (`lib/pleroma/user/notification_setting.ex`): both flags live in the
//! account's `notification_settings` jsonb column, the endpoint answers
//! `{"status":"success"}`, and a submission names only what it changes.
//!
//! - `PUT /api/pleroma/notification_settings` (scope `write:accounts`) —
//!   body parameters `block_from_strangers` and `hide_notification_contents`,
//!   each a boolean (JSON `true`/`false` or the form spellings `flex`
//!   accepts; anything else is 422). A present parameter updates that flag,
//!   an absent one keeps the stored value, and an empty body changes
//!   nothing without being an error. The answer is `{"status":"success"}`.
//!   `block_from_strangers` is applied when notifications are created (see
//!   [`maybe_notify`](crate::models::notification::maybe_notify));
//!   `hide_notification_contents` is stored for the later push-delivery
//!   ticket to read.

use axum::{Json, Router, extract::State, routing::put};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    auth::AuthUser,
    error::AppResult,
    extract::{ApiBody, flex},
    models::user::notification_settings::{self, NotificationSettings},
    oauth::scopes::WRITE_ACCOUNTS,
    state::AppState,
};

/// The body of `PUT /api/pleroma/notification_settings`: the two flags,
/// each optional so an absent one leaves the stored value alone.
#[derive(Debug, Deserialize)]
struct NotificationSettingsParams {
    /// The new `block_from_strangers` flag, or `None` to keep it.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    block_from_strangers: Option<bool>,
    /// The new `hide_notification_contents` flag, or `None` to keep it.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    hide_notification_contents: Option<bool>,
}

/// The `/api/pleroma` notification-settings router, merged into
/// [`crate::api::pleroma`].
///
/// Do not call [`Router::fallback`] here: unmatched paths must reach the
/// app-wide JSON fallback in [`crate::app`].
pub fn router() -> Router<AppState> {
    Router::new().route("/notification_settings", put(update))
}

/// `PUT /api/pleroma/notification_settings` — update the flags the body
/// names and answer `{"status":"success"}`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check; [`AppError::Unprocessable`] from the
/// extractor for a flag that is neither a JSON boolean nor one of the
/// form spellings. Any database failure arrives through `?` as
/// [`AppError::Database`].
///
/// [`AppError::Unauthorized`]: crate::error::AppError::Unauthorized
/// [`AppError::Forbidden`]: crate::error::AppError::Forbidden
/// [`AppError::Unprocessable`]: crate::error::AppError::Unprocessable
/// [`AppError::Database`]: crate::error::AppError::Database
async fn update(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<NotificationSettingsParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    let settings = apply(&params, &NotificationSettings::for_user(&auth.0));
    notification_settings::save(&state.db, auth.0.id, &settings).await?;
    Ok(Json(json!({ "status": "success" })))
}

/// The stored `settings` with each flag `params` names replaced (present
/// only: Pleroma's changeset leaves an unnamed field at its stored value).
#[must_use]
fn apply(
    params: &NotificationSettingsParams,
    settings: &NotificationSettings,
) -> NotificationSettings {
    NotificationSettings {
        block_from_strangers: params
            .block_from_strangers
            .unwrap_or(settings.block_from_strangers),
        hide_notification_contents: params
            .hide_notification_contents
            .unwrap_or(settings.hide_notification_contents),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The params of a body that names both flags, as `{"block": … ,
    /// "hide": …}` would spell them.
    fn both(flags: (bool, bool)) -> NotificationSettingsParams {
        serde_json::from_value(json!({
            "block_from_strangers": flags.0,
            "hide_notification_contents": flags.1,
        }))
        .expect("both flags named as booleans")
    }

    #[test]
    fn present_params_replace_only_the_named_flags() {
        let stored = NotificationSettings {
            block_from_strangers: true,
            hide_notification_contents: false,
        };
        assert_eq!(
            apply(
                &NotificationSettingsParams {
                    block_from_strangers: Some(false),
                    hide_notification_contents: None,
                },
                &stored,
            ),
            NotificationSettings {
                block_from_strangers: false,
                hide_notification_contents: false,
            },
            "the named flag flips, the unnamed one stays"
        );
        assert_eq!(
            apply(
                &NotificationSettingsParams {
                    block_from_strangers: None,
                    hide_notification_contents: Some(true),
                },
                &stored,
            ),
            NotificationSettings {
                block_from_strangers: true,
                hide_notification_contents: true,
            },
            "and the same holds for the other flag"
        );
        assert_eq!(
            apply(
                &NotificationSettingsParams {
                    block_from_strangers: None,
                    hide_notification_contents: None,
                },
                &stored,
            ),
            stored,
            "an empty body changes nothing"
        );
        assert_eq!(
            apply(&both((false, true)), &stored),
            NotificationSettings {
                block_from_strangers: false,
                hide_notification_contents: true,
            },
            "both flags can move at once"
        );
    }

    #[test]
    fn params_deserialize_from_json_booleans() {
        let parsed = serde_json::from_value::<NotificationSettingsParams>(json!({
            "block_from_strangers": true,
            "hide_notification_contents": false,
        }))
        .expect("real booleans parse");
        assert_eq!(
            parsed.block_from_strangers,
            Some(true),
            "the block flag reads"
        );
        assert_eq!(
            parsed.hide_notification_contents,
            Some(false),
            "and so does the hide flag"
        );
    }

    #[test]
    fn params_deserialize_from_form_strings_and_nulls() {
        // The shape `ApiBody` builds from `block_from_strangers=on`.
        let parsed = serde_json::from_value::<NotificationSettingsParams>(json!({
            "block_from_strangers": "on",
            "hide_notification_contents": "FALSE",
        }))
        .expect("form spellings parse");
        assert_eq!(
            (
                parsed.block_from_strangers,
                parsed.hide_notification_contents
            ),
            (Some(true), Some(false)),
            "the flex spellings coerce like the follow toggles"
        );
        let nulled = serde_json::from_value::<NotificationSettingsParams>(json!({
            "block_from_strangers": null,
        }))
        .expect("a null is no parameter");
        assert_eq!(
            (
                nulled.block_from_strangers,
                nulled.hide_notification_contents
            ),
            (None, None),
            "null keeps both flags unnamed"
        );
    }

    #[test]
    fn an_invalid_boolean_is_a_parse_failure() {
        for value in [
            json!({"block_from_strangers": "maybe"}),
            json!({"hide_notification_contents": 1}),
            json!({"block_from_strangers": ["true"]}),
        ] {
            assert!(
                serde_json::from_value::<NotificationSettingsParams>(value.clone()).is_err(),
                "`{value}` names no boolean",
            );
        }
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let parsed = serde_json::from_value::<NotificationSettingsParams>(json!({
            "block_from_strangers": true,
            "some_other_setting": "whatever",
        }))
        .expect("the known flag parses");
        assert_eq!(
            parsed.block_from_strangers,
            Some(true),
            "the rest is none of this endpoint's business"
        );
    }
}
