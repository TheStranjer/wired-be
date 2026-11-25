//! `GET /api/v1/preferences`: the account's posting and reading defaults.
//!
//! Mastodon clients read it right after login to learn the default
//! visibility, sensitivity and language of a new status. The answer is the
//! flat `{"posting:default:visibility": …}` object of Mastodon's
//! `Api::V1::PreferencesController`, built from what the row stores and
//! kept in step with the `source` block of `verify_credentials`
//! ([`AccountSource::for_owner`]): `posting:default:visibility` is
//! `source.privacy`, `posting:default:sensitive` is `source.sensitive` and
//! `posting:default:language` is `source.language`. No per-account default
//! quote policy is stored, so `posting:default:quote_policy` is always
//! [`DEFAULT_QUOTE_POLICY`], and the three `reading:*` keys are the static
//! defaults Mastodon answers for an account that never changed them.
//!
//! The route needs a user token with the `read:accounts` scope.
//!
//! [`AccountSource::for_owner`]: crate::entities::account::AccountSource::for_owner

use std::future::{Ready, ready};

use axum::{Json, Router, routing::get};
use serde::Serialize;

use crate::{
    auth::AuthUser, error::AppResult, models::user::User, oauth::scopes::READ_ACCOUNTS,
    state::AppState,
};

/// Who may quote a new status by default: nobody narrows it, so everyone.
pub const DEFAULT_QUOTE_POLICY: &str = "public";
/// How media is expanded by default: the client decides.
pub const DEFAULT_EXPAND_MEDIA: &str = "default";

/// The `posting:*` half of the [`Preferences`] object.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct PostingDefaults {
    /// `posting:default:visibility`: the visibility new statuses default to.
    #[serde(rename = "posting:default:visibility")]
    pub visibility: String,
    /// `posting:default:sensitive`: whether new statuses default to sensitive.
    #[serde(rename = "posting:default:sensitive")]
    pub sensitive: bool,
    /// `posting:default:language`: the language tag new statuses default to.
    #[serde(rename = "posting:default:language")]
    pub language: Option<String>,
    /// `posting:default:quote_policy`: who may quote new statuses by default.
    #[serde(rename = "posting:default:quote_policy")]
    pub quote_policy: String,
}

/// The `reading:*` half of the [`Preferences`] object.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct ReadingDefaults {
    /// `reading:expand:media`: how clients expand media.
    #[serde(rename = "reading:expand:media")]
    pub expand_media: String,
    /// `reading:expand:spoilers`: whether clients expand content warnings.
    #[serde(rename = "reading:expand:spoilers")]
    pub expand_spoilers: bool,
    /// `reading:autoplay:gifs`: whether clients autoplay animated images.
    #[serde(rename = "reading:autoplay:gifs")]
    pub autoplay_gifs: bool,
}

/// The preferences object `GET /api/v1/preferences` answers with.
///
/// The two halves are flattened into one flat JSON object.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Preferences {
    /// The `posting:*` keys.
    #[serde(flatten)]
    pub posting: PostingDefaults,
    /// The `reading:*` keys.
    #[serde(flatten)]
    pub reading: ReadingDefaults,
}

impl From<User> for Preferences {
    /// The preferences of `user`, off the same columns as the `source` block.
    fn from(user: User) -> Self {
        Self {
            posting: PostingDefaults {
                visibility: user.default_scope,
                sensitive: user.sensitive_default.get(),
                language: user.language,
                quote_policy: DEFAULT_QUOTE_POLICY.to_owned(),
            },
            reading: ReadingDefaults {
                expand_media: DEFAULT_EXPAND_MEDIA.to_owned(),
                expand_spoilers: false,
                autoplay_gifs: false,
            },
        }
    }
}

/// The `/api/v1/preferences` router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new().route("/preferences", get(show))
}

/// `GET /api/v1/preferences` — the caller's posting and reading defaults.
///
/// Nothing here waits, so the future is already resolved.
///
/// # Errors
///
/// [`Unauthorized`](crate::error::AppError::Unauthorized) /
/// [`Forbidden`](crate::error::AppError::Forbidden) from
/// [`AuthUser`] and the `read:accounts` check.
fn show(auth: AuthUser) -> Ready<AppResult<Json<Preferences>>> {
    ready(
        auth.require(READ_ACCOUNTS)
            .map(|()| Json(Preferences::from(auth.0))),
    )
}
