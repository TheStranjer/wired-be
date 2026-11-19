//! Merging one `update_credentials` body over the stored row into the
//! [`ProfileFields`](crate::models::user::ProfileFields) the write
//! takes: [`profile_fields`] folds the whole body, and the helpers here
//! cover the Mastodon columns, the privacy default and the language
//! tag; the Pleroma-side pairings of the same fold — the preference
//! flags are here, the birthday, settings blob and actor kind are
//! [`credentials_pleroma`]'s.
//!
//! [`credentials_pleroma`]: super::credentials_pleroma
//!
//! The posting defaults arrive in Mastodon's `source` block —
//! `source[privacy]`, `source[sensitive]` and `source[language]` as form keys
//! or as the nested JSON object of the same shape — and Pleroma's flat
//! `default_scope` is the other spelling of the privacy one, losing to
//! `source[privacy]` when a body carries both, as in
//! `AccountController.update_credentials`. `privacy` must be one of the four
//! visibilities in `SCOPES` (any other text is the 422 `PRIVACY_INVALID`
//! wording), `sensitive` is a [`flex`] boolean, and `language` is a tag of
//! at most `MAX_LANGUAGE_CHARS` characters whose empty string clears the
//! column.

use chrono::Utc;

use crate::{
    error::{AppError, AppResult},
    models::user::{Flag, PleromaPrefs, ProfileFields, User},
    state::AppState,
};

use super::{
    SCOPES,
    credentials::ProfileParams,
    credentials_pleroma::{merged_actor, merged_birthday, merged_store},
    profile_text::{render_note, sanitized_name, stored_fields},
};

/// The 422 body for a `source[privacy]` (or the flat `default_scope` of the
/// same parameter) outside [`SCOPES`], in the wording the scrobble endpoint
/// uses for its visibility.
const PRIVACY_INVALID: &str = "privacy: is not included in the list";

/// The longest `source[language]` tag accepted, in characters; longer is the
/// 422 in [`merged_language`]'s wording.
const MAX_LANGUAGE_CHARS: usize = 10;

/// Validates the present profile parameters and merges them over the
/// stored row: every column
/// [`update_profile`](crate::models::user::update_profile) will write, with
/// fields absent from the request carrying the value they already hold and
/// fields present-but-blank carrying the blank (an empty `display_name` is
/// stored as `""`, not reset to the nickname — this endpoint's ticket pins
/// that against Pleroma's registration-side fallback). A `note` over
/// [`max_bio_chars`](Config::max_bio_chars) characters (counted in
/// characters, not bytes) is the 422 mirroring the name message; a submitted
/// `fields_attributes` is validated and rendered by [`stored_fields`]. The
/// Pleroma side of the body merges through the `merged_*` helpers below:
/// [`merged_scope`] and [`merged_language`] for two of the three posting
/// defaults (`source[sensitive]` is a plain [`flex`] boolean),
/// [`merged_prefs`] for the eleven preference flags, [`merged_birthday`],
/// [`merged_store`] and [`merged_actor`] for the date, the client settings
/// blob and the actor kind. The three image columns start from the stored
/// row here and are overwritten by [`ImageWrites::apply`], which is the only
/// writer that can store a file.
///
/// # Errors
///
/// [`AppError::Unprocessable`] from [`sanitized_name`], the note length
/// check, [`stored_fields`], [`merged_scope`], [`merged_language`],
/// [`merged_actor`], [`merged_birthday`] or [`merged_store`];
/// [`render_note`] adds any database failure as [`AppError::Database`].
pub(super) async fn profile_fields(
    state: &AppState,
    current: &User,
    params: ProfileParams,
) -> AppResult<ProfileFields> {
    let name = params
        .display_name
        .as_deref()
        .map(|raw| sanitized_name(raw, state.config.max_display_name_chars))
        .transpose()?;
    if params
        .note
        .as_deref()
        .is_some_and(|text| text.chars().count() > state.config.max_bio_chars)
    {
        return Err(AppError::Unprocessable(format!(
            "Bio is too long (maximum is {} characters)",
            state.config.max_bio_chars
        )));
    }
    let (bio, raw_bio) = match params.note.as_deref() {
        Some(raw) => (render_note(state, raw).await?, raw.to_owned()),
        None => (current.bio.clone(), current.raw_bio.clone()),
    };
    let (fields, raw_fields) = match params.fields_attributes.as_ref() {
        Some(submitted) => stored_fields(submitted.clone(), state.config.max_profile_fields)?,
        None => (current.fields.clone(), current.raw_fields.clone()),
    };
    let privacy = params
        .source
        .as_ref()
        .and_then(|nested| nested.privacy.as_deref())
        .or(params.default_scope.as_deref());
    let language = params
        .source
        .as_ref()
        .and_then(|nested| nested.language.as_deref());
    let sensitive = params.source.as_ref().and_then(|nested| nested.sensitive);
    let (actor_type, bot) = merged_actor(&params, current)?;
    Ok(ProfileFields {
        name: name.unwrap_or_else(|| current.name.clone()),
        bio,
        raw_bio,
        locked: params.locked.map_or(current.locked, Flag::from),
        bot,
        discoverable: params.discoverable.map_or(current.discoverable, Flag::from),
        fields,
        raw_fields,
        avatar_url: current.avatar_url.clone(),
        banner_url: current.banner_url.clone(),
        background_image_url: current.background_image_url.clone(),
        default_scope: merged_scope(privacy, &current.default_scope)?,
        sensitive_default: sensitive.map_or(current.sensitive_default, Flag::from),
        language: merged_language(language, current.language.as_deref())?,
        pleroma: merged_prefs(&params, current),
        birthday: merged_birthday(
            params.birthday.as_deref(),
            current.birthday,
            Utc::now().date_naive(),
        )?,
        settings_store: merged_store(
            params.pleroma_settings_store.as_ref(),
            &current.settings_store,
        )?,
        actor_type,
    })
}

/// The `default_scope` to store: `submitted` — the body's
/// `source[privacy]`, which wins over Pleroma's flat `default_scope` because
/// that is the order `update_credentials` applies them — checked against
/// [`SCOPES`]; `None` (neither parameter, or a JSON `null`) keeps the stored
/// visibility.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`PRIVACY_INVALID`] for any other text.
fn merged_scope(submitted: Option<&str>, stored: &str) -> AppResult<String> {
    match submitted {
        Some(text) if SCOPES.contains(&text) => Ok(text.to_owned()),
        Some(_) => Err(AppError::Unprocessable(PRIVACY_INVALID.to_owned())),
        None => Ok(stored.to_owned()),
    }
}

/// The `language` to store: `submitted` — the body's `source[language]`, at
/// most [`MAX_LANGUAGE_CHARS`] characters counted in characters, not bytes —
/// with the empty string clearing the preference to `None`; `None` (no
/// parameter) keeps the stored tag.
///
/// # Errors
///
/// [`AppError::Unprocessable`] in [`sanitized_name`]'s wording for an
/// over-long tag.
fn merged_language(submitted: Option<&str>, stored: Option<&str>) -> AppResult<Option<String>> {
    let Some(raw) = submitted else {
        return Ok(stored.map(str::to_owned));
    };
    if raw.chars().count() > MAX_LANGUAGE_CHARS {
        return Err(AppError::Unprocessable(format!(
            "Language is too long (maximum is {MAX_LANGUAGE_CHARS} characters)"
        )));
    }
    Ok((!raw.is_empty()).then(|| raw.to_owned()))
}

/// The eleven `pleroma` preference flags after this body: every one of them
/// the submitted [`flex`] boolean or, absent, the flag the row holds. No
/// validation beyond the coercion itself: these are plain booleans.
fn merged_prefs(params: &ProfileParams, current: &User) -> PleromaPrefs {
    PleromaPrefs {
        hide_followers: params
            .hide_followers
            .map_or(current.hide_followers, Flag::from),
        hide_follows: params.hide_follows.map_or(current.hide_follows, Flag::from),
        hide_followers_count: params
            .hide_followers_count
            .map_or(current.hide_followers_count, Flag::from),
        hide_follows_count: params
            .hide_follows_count
            .map_or(current.hide_follows_count, Flag::from),
        hide_favorites: params
            .hide_favorites
            .map_or(current.hide_favorites, Flag::from),
        show_role: params.show_role.map_or(current.show_role, Flag::from),
        skip_thread_containment: params
            .skip_thread_containment
            .map_or(current.skip_thread_containment, Flag::from),
        allow_following_move: params
            .allow_following_move
            .map_or(current.allow_following_move, Flag::from),
        accepts_chat_messages: params
            .accepts_chat_messages
            .map_or(current.accepts_chat_messages, Flag::from),
        no_rich_text: params.no_rich_text.map_or(current.no_rich_text, Flag::from),
        show_birthday: params
            .show_birthday
            .map_or(current.show_birthday, Flag::from),
    }
}

#[cfg(test)]
mod tests {
    use crate::api::accounts::test_support::{params, stored_user};

    use super::*;

    #[test]
    fn privacy_is_a_scope_or_the_422() {
        assert_eq!(
            merged_scope(Some("direct"), "public").unwrap(),
            "direct",
            "a stated scope wins over the stored one"
        );
        assert_eq!(
            merged_scope(None, "private").unwrap(),
            "private",
            "no parameter keeps the stored scope"
        );
        for text in ["internal", "", "PUBLIC", "unlisted "] {
            let err = merged_scope(Some(text), "public").expect_err("a scope outside the list");
            assert_eq!(err.to_string(), PRIVACY_INVALID, "the pinned wording");
        }
    }

    #[test]
    fn a_language_tag_is_capped_and_the_empty_one_clears() {
        assert_eq!(
            merged_language(Some("pt-BR"), Some("de"))
                .unwrap()
                .as_deref(),
            Some("pt-BR"),
            "a submitted tag replaces the stored one"
        );
        assert_eq!(
            merged_language(Some(""), Some("pt-BR")).unwrap(),
            None,
            "the empty string clears the preference"
        );
        assert_eq!(
            merged_language(None, Some("pt-BR")).unwrap().as_deref(),
            Some("pt-BR"),
            "and no parameter keeps it"
        );
        assert_eq!(
            merged_language(None, None).unwrap(),
            None,
            "nothing stated is nothing stored"
        );
        let at_cap = "é".repeat(MAX_LANGUAGE_CHARS);
        assert_eq!(
            merged_language(Some(at_cap.as_str()), None)
                .unwrap()
                .as_deref(),
            Some(at_cap.as_str()),
            "exactly the cap fits, counted in characters, not bytes"
        );
        let err = merged_language(Some("0123456789a"), None).expect_err("eleven is over the cap");
        assert_eq!(
            err.to_string(),
            "Language is too long (maximum is 10 characters)",
            "the length message names the cap"
        );
    }

    #[test]
    fn each_pleroma_flag_merges_on_its_own() {
        let current = stored_user();
        let body = params(r#"{"hide_follows": "on", "no_rich_text": false}"#);
        let prefs = merged_prefs(&body, &current);
        assert!(prefs.hide_follows.get(), "the submitted flag is set");
        assert!(!prefs.no_rich_text.get(), "and so is the submitted false");
        assert!(
            prefs.hide_followers.get() && prefs.hide_favorites.get(),
            "flags the body never carried keep what the row held"
        );
        assert!(
            prefs.show_role.get()
                && prefs.allow_following_move.get()
                && prefs.accepts_chat_messages.get(),
            "the on-by-default flags survive an unrelated write"
        );
        assert!(
            !prefs.show_birthday.get()
                && !prefs.skip_thread_containment.get()
                && !prefs.hide_followers_count.get()
                && !prefs.hide_follows_count.get(),
            "and so do the off-by-default ones"
        );
    }
}
