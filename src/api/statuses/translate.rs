//! # The translation
//!
//! `POST /api/v1/statuses/{id}/translate` is Pleroma's `translate`: a user
//! token with `read:statuses`, the same [`status_for_viewer_or_404`] gate (so
//! a post the caller may not read is the shared 404, where Pleroma answers its
//! visibility arm 404 too), and the body's `lang` as the target language —
//! falling back to the caller's own `language` and then to `en`. Which
//! provider that asks, and what its call looks like on the wire, is
//! [`crate::services::translate`]'s business; this route adds only the gate,
//! the answer's [`Translation`] entity and the two 503s an instance without a
//! working provider owes: `"Translation is not enabled"` while none is
//! configured, `"Translation service unavailable"` when the configured one
//! fails.
//!
//! [`status_for_viewer_or_404`]: crate::api::helpers::status_for_viewer_or_404
//! [`Translation`]: crate::entities::translation::Translation

use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
use tracing::{debug, warn};

use crate::{
    api::helpers::status_for_viewer_or_404,
    auth::AuthUser,
    entities::translation::Translation,
    error::{AppError, AppResult},
    extract::ApiBody,
    oauth::scopes::READ_STATUSES,
    services::translate::Translator,
    state::AppState,
};

use super::{DEFAULT_TARGET_LANGUAGE, NOT_ENABLED, UNAVAILABLE};

/// `POST /api/v1/statuses/{id}/translate` — translate the status `id` into
/// the body's `lang` and answer the [`Translation`] entity.
///
/// The status resolves through [`status_for_viewer_or_404`] like every other
/// route keyed by id: Pleroma's `translate` refuses anything but a `public`
/// or `unlisted` post, and here the visibility model's answer is the shared
/// 404, so a private post translation cannot see is also a post that
/// translation cannot say exists. What is translated is the status's HTML
/// `content`, in the language the provider's own protocol offers for it, and
/// the answer's `detected_source_language` is what the provider says the
/// original was written in.
///
/// Without a configured provider the call cannot succeed: 503
/// `{"error":"Translation is not enabled"}`. A provider that is configured
/// but does not answer — transport failure, timeout, or a 4xx/5xx of its own —
/// is 503 `{"error":"Translation service unavailable"}`, with the real reason
/// in the log and never in the body.
///
/// [`Translation`]: crate::entities::translation::Translation
pub(super) async fn translate_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(params): ApiBody<TranslateParams>,
) -> AppResult<Json<Translation>> {
    auth.require(READ_STATUSES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    let translator = Translator::from_config(&state.config, &state.http_client);
    if translator.provider_name().is_none() {
        return Err(AppError::ServiceUnavailable(NOT_ENABLED.to_owned()));
    }
    let target = target_language(params.lang.as_deref(), auth.0.language.as_deref());
    let translated = translator
        .translate(&row.content, row.language.as_deref(), &target)
        .await
        .map_err(|err| {
            warn!(
                %err,
                status = %row.id,
                target = %target,
                "status translation failed"
            );
            AppError::ServiceUnavailable(UNAVAILABLE.to_owned())
        })?;
    debug!(
        status = %row.id,
        provider = translated.provider,
        target = %target,
        "status translated"
    );
    Ok(Json(Translation::for_status(&row, translated)))
}

/// The body of `POST /api/v1/statuses/{id}/translate`.
#[derive(Debug, Deserialize)]
pub(super) struct TranslateParams {
    /// The language to translate into; absent (or blank) asks for the
    /// caller's own default — see [`target_language`].
    #[serde(default)]
    lang: Option<String>,
}

/// The language `POST …/translate` moves a status into: the body's `lang`,
/// else the caller's account default — the `users.language` column the
/// `Status` entity shows as `source.language` — else
/// [`DEFAULT_TARGET_LANGUAGE`]. That is Pleroma's
/// `Map.get(params, :lang) || user.language` with a fallback for the account
/// that never named a language, since a blank target is no language to ask a
/// provider for; a blank spelling of either is no language either.
fn target_language(requested: Option<&str>, preferred: Option<&str>) -> String {
    let asked = requested.map(str::trim).filter(|lang| !lang.is_empty());
    let default = preferred.map(str::trim).filter(|lang| !lang.is_empty());
    asked
        .or(default)
        .unwrap_or(DEFAULT_TARGET_LANGUAGE)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    /// The body of a translate request, as `ApiBody` would hand it over:
    /// parsed from the JSON shape the extractor builds for either accepted
    /// content type.
    fn params(body: Value) -> TranslateParams {
        serde_json::from_value(body).expect("the extractor's shape parses")
    }

    #[test]
    fn the_body_carries_one_optional_target_language() {
        assert_eq!(
            params(json!({"lang": "de"})).lang.as_deref(),
            Some("de"),
            "the documented key"
        );
        assert_eq!(
            params(json!({})).lang,
            None,
            "an empty body — which is also what an absent body and a bare              `lang=` form parameter stand for — names no language"
        );
    }

    #[test]
    fn the_asked_language_beats_the_account_default() {
        assert_eq!(
            target_language(Some("eo"), Some("de")),
            "eo",
            "the body's `lang` wins"
        );
        assert_eq!(
            target_language(None, Some("de")),
            "de",
            "without it the caller's own default"
        );
        assert_eq!(
            target_language(Some(" fr "), Some("de")),
            "fr",
            "and a padded spelling is trimmed, not rejected"
        );
    }

    #[test]
    fn english_is_the_last_resort() {
        for (asked, preferred) in [
            (None, None),
            (Some(""), None),
            (Some("   "), Some("")),
            (None, Some("  ")),
        ] {
            assert_eq!(
                target_language(asked, preferred),
                "en",
                "a blank is no language to ask a provider for, so `{asked:?}`                  with `{preferred:?}` falls back to English"
            );
        }
    }
}
