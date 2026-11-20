//! The field checks a credentials patch runs: the stored profile and
//! flags, and the name, note, metadata, scope, actor and store checks.

use serde_json::Value;

use super::credentials::CredentialsParams;
use super::{INCLUSION, STORE_INVALID, refused};
use crate::{
    api::{
        accounts::{
            ACTOR_TYPES, SCOPES, SERVICE_ACTOR_TYPE, render_note, sanitized_name, stored_fields,
        },
        pleroma_settings::{check_store_size, shallow_merge},
    },
    config::Config,
    error::{AppError, AppResult},
    models::user::{Flag, PleromaPrefs, ProfileFields, User, register},
    state::AppState,
};

/// The checked profile submissions of one credentials body, `None` each for
/// the field it does not write.
#[derive(Debug)]
pub(super) struct ProfileChecks {
    /// The sanitized display name for the `name` column.
    pub(super) name: Option<String>,
    /// The note's submitted source text, rendered by [`merge_into`].
    pub(super) note: Option<String>,
    /// The stored `(fields, raw_fields)` pair from [`stored_fields`].
    pub(super) metadata: Option<(Value, Value)>,
    /// The visibility the `default_scope` column takes.
    pub(super) scope: Option<String>,
    /// The `(actor_type, bot)` pair an explicit kind decides.
    pub(super) actor: Option<(String, Flag)>,
    /// The merged client settings blob.
    pub(super) store: Option<Value>,
}

impl ProfileChecks {
    /// Whether this body writes no profile column at all: nothing checked,
    /// no flag stated.
    pub(super) fn untouched(&self, params: &CredentialsParams) -> bool {
        self.name.is_none()
            && self.note.is_none()
            && self.metadata.is_none()
            && self.scope.is_none()
            && self.actor.is_none()
            && self.store.is_none()
            && !params.any_flag()
    }

    /// The stored row's [`ProfileFields`] — every column carrying its stored
    /// value, since a credentials PATCH rewrites the whole set at once —
    /// with every checked submission written on top. Only the note needs a
    /// database read to render, so it lands last.
    ///
    /// # Errors
    ///
    /// [`AppError::Database`] from [`render_note`].
    pub(super) async fn merge_into(
        self,
        state: &AppState,
        current: &User,
        params: &CredentialsParams,
    ) -> AppResult<ProfileFields> {
        let mut profile = stored_profile(current);
        if let Some(name) = self.name {
            profile.name = name;
        }
        if let Some(raw) = self.note.as_deref() {
            profile.bio = render_note(state, raw).await?;
            profile.raw_bio = raw.to_owned();
        }
        if let Some((fields, raw_fields)) = self.metadata {
            profile.fields = fields;
            profile.raw_fields = raw_fields;
        }
        if let Some(scope) = self.scope {
            profile.default_scope = scope;
        }
        if let Some((kind, automated)) = self.actor {
            profile.actor_type = kind;
            profile.bot = automated;
        }
        if let Some(store) = self.store {
            profile.settings_store = store;
        }
        merge_flags(params, &mut profile);
        Ok(profile)
    }
}

/// The stored row as a complete [`ProfileFields`]: the shape
/// [`update_profile`] wants when a request changes columns one by one.
///
/// [`update_profile`]: crate::models::user::update_profile
pub(super) fn stored_profile(current: &User) -> ProfileFields {
    ProfileFields {
        name: current.name.clone(),
        bio: current.bio.clone(),
        raw_bio: current.raw_bio.clone(),
        locked: current.locked,
        bot: current.bot,
        discoverable: current.discoverable,
        fields: current.fields.clone(),
        raw_fields: current.raw_fields.clone(),
        avatar_url: current.avatar_url.clone(),
        banner_url: current.banner_url.clone(),
        background_image_url: current.background_image_url.clone(),
        default_scope: current.default_scope.clone(),
        sensitive_default: current.sensitive_default,
        language: current.language.clone(),
        pleroma: stored_flags(current),
        birthday: current.birthday,
        settings_store: current.settings_store.clone(),
        actor_type: current.actor_type.clone(),
    }
}

/// The eleven `pleroma` preference columns as the row holds them; the nine
/// a credentials body owns are overwritten by [`merge_flags`].
pub(super) const fn stored_flags(current: &User) -> PleromaPrefs {
    PleromaPrefs {
        hide_followers: current.hide_followers,
        hide_follows: current.hide_follows,
        hide_followers_count: current.hide_followers_count,
        hide_follows_count: current.hide_follows_count,
        hide_favorites: current.hide_favorites,
        show_role: current.show_role,
        skip_thread_containment: current.skip_thread_containment,
        allow_following_move: current.allow_following_move,
        accepts_chat_messages: current.accepts_chat_messages,
        no_rich_text: current.no_rich_text,
        show_birthday: current.show_birthday,
    }
}

/// Writes the stated preference booleans over the merged profile: the two
/// named `is_…` in the body's spelling, the nine in the entity's.
pub(super) fn merge_flags(params: &CredentialsParams, profile: &mut ProfileFields) {
    profile.locked = params.is_locked.map_or(profile.locked, Flag::from);
    profile.discoverable = params
        .is_discoverable
        .map_or(profile.discoverable, Flag::from);
    let pleroma = &mut profile.pleroma;
    pleroma.hide_followers = params
        .hide_followers
        .map_or(pleroma.hide_followers, Flag::from);
    pleroma.hide_follows = params.hide_follows.map_or(pleroma.hide_follows, Flag::from);
    pleroma.hide_followers_count = params
        .hide_followers_count
        .map_or(pleroma.hide_followers_count, Flag::from);
    pleroma.hide_follows_count = params
        .hide_follows_count
        .map_or(pleroma.hide_follows_count, Flag::from);
    pleroma.hide_favorites = params
        .hide_favorites
        .map_or(pleroma.hide_favorites, Flag::from);
    pleroma.show_role = params.show_role.map_or(pleroma.show_role, Flag::from);
    pleroma.skip_thread_containment = params
        .skip_thread_containment
        .map_or(pleroma.skip_thread_containment, Flag::from);
    pleroma.allow_following_move = params
        .allow_following_move
        .map_or(pleroma.allow_following_move, Flag::from);
    pleroma.no_rich_text = params.no_rich_text.map_or(pleroma.no_rich_text, Flag::from);
}

/// The checked display name: [`sanitized_name`] ran the user endpoint's
/// length rule and stripping, and its refusal lands under `name`.
///
/// # Errors
///
/// [`AppError::Database`] never (the check is pure); any non-`Unprocessable`
/// failure the helper could one day carry still aborts the request.
pub(super) fn checked_name(
    submitted: Option<&str>,
    config: &Config,
    errors: &mut register::FieldErrors,
) -> AppResult<Option<String>> {
    let Some(raw) = submitted else {
        return Ok(None);
    };
    match sanitized_name(raw, config.max_display_name_chars) {
        Ok(clean) => Ok(Some(clean)),
        Err(AppError::Unprocessable(message)) => {
            refused(errors, "name", message);
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

/// The note's submission after the user endpoint's length rule, whose
/// message lands under `bio`; the rendering waits for [`merge_into`].
pub(super) fn checked_note(
    submitted: Option<&str>,
    config: &Config,
    errors: &mut register::FieldErrors,
) -> Option<String> {
    let raw = submitted?;
    if raw.chars().count() > config.max_bio_chars {
        refused(
            errors,
            "bio",
            format!(
                "Bio is too long (maximum is {} characters)",
                config.max_bio_chars
            ),
        );
        return None;
    }
    Some(raw.to_owned())
}

/// The checked metadata pair: [`stored_fields`] ran the count and length
/// rules, and its refusal lands under `raw_fields` — the body's own name
/// for the input.
///
/// # Errors
///
/// None in practice: the helper's only failure is the collected
/// `Unprocessable`, but any other still aborts the request.
pub(super) fn checked_metadata(
    submitted: Option<Value>,
    config: &Config,
    errors: &mut register::FieldErrors,
) -> AppResult<Option<(Value, Value)>> {
    let Some(raw) = submitted else {
        return Ok(None);
    };
    match stored_fields(raw, config.max_profile_fields) {
        Ok(pair) => Ok(Some(pair)),
        Err(AppError::Unprocessable(message)) => {
            refused(errors, "raw_fields", message);
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

/// The checked visibility: any text outside [`SCOPES`] is Ecto's inclusion
/// message under `default_scope`, and `None` keeps the stored one.
pub(super) fn checked_scope<'a>(
    submitted: Option<&'a str>,
    errors: &mut register::FieldErrors,
) -> Option<&'a str> {
    match submitted {
        Some(scope) if SCOPES.contains(&scope) => Some(scope),
        Some(_) => {
            refused(errors, "default_scope", INCLUSION);
            None
        }
        None => None,
    }
}

/// The checked actor kind: outside [`ACTOR_TYPES`] the same inclusion
/// message under `actor_type`, inside it the pair the columns keep in step
/// — Pleroma's admin write re-decides `bot` from the kind.
pub(super) fn checked_actor(
    submitted: Option<&str>,
    errors: &mut register::FieldErrors,
) -> Option<(String, Flag)> {
    match submitted {
        Some(kind) if ACTOR_TYPES.contains(&kind) => {
            Some((kind.to_owned(), Flag::new(kind == SERVICE_ACTOR_TYPE)))
        }
        Some(_) => {
            refused(errors, "actor_type", INCLUSION);
            None
        }
        None => None,
    }
}

/// The checked client settings blob: the submitted object (or the JSON text
/// of one, from a form) shallow-merged over the stored blob exactly as the
/// user endpoint merges it, then measured. A value that is no object is
/// Ecto's `is invalid`, a merged blob over the cap is `is too large` — both
/// under `pleroma_settings_store`, and both leaving the stored blob
/// untouched because the whole request is refused.
pub(super) fn checked_store(
    submitted: Option<&Value>,
    current: &User,
    errors: &mut register::FieldErrors,
) -> Option<Value> {
    let patch = submitted?;
    let Value::Object(map) = store_value(patch) else {
        refused(errors, "pleroma_settings_store", STORE_INVALID);
        return None;
    };
    let merged = shallow_merge(&current.settings_store, &map);
    if check_store_size(&merged).is_ok() {
        return Some(merged);
    }
    refused(errors, "pleroma_settings_store", "is too large");
    None
}

/// The value a submitted `pleroma_settings_store` names once a form's JSON
/// text has been parsed: the text of an object becomes the object, text
/// that is no JSON folds to [`Value::Null`], and every other value comes
/// back as it stands for [`checked_store`]'s object test to refuse.
pub(super) fn store_value(submitted: &Value) -> Value {
    if let Value::String(text) = submitted {
        return serde_json::from_str::<Value>(text).unwrap_or(Value::Null);
    }
    submitted.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The scope and actor-kind checks keep to their lists — and the kind
    /// re-decides `bot` exactly as the columns must agree.
    #[test]
    fn the_list_checks_refuse_what_is_not_on_their_list() {
        let mut errors = register::FieldErrors::new();
        assert_eq!(
            checked_scope(Some("private"), &mut errors),
            Some("private"),
            "an ordinary visibility passes"
        );
        assert_eq!(
            checked_scope(None, &mut errors),
            None,
            "absence keeps the stored one"
        );
        assert_eq!(
            checked_scope(Some("leaked"), &mut errors),
            None,
            "off the list"
        );

        assert_eq!(
            checked_actor(Some("Service"), &mut errors),
            Some(("Service".to_owned(), Flag::new(true))),
            "the automated kind makes a bot"
        );
        assert_eq!(
            checked_actor(Some("Person"), &mut errors),
            Some(("Person".to_owned(), Flag::new(false))),
            "and the human one unmakes it"
        );
        assert_eq!(
            checked_actor(Some("Group"), &mut errors),
            None,
            "Group is not settable"
        );
        assert_eq!(
            errors,
            register::FieldErrors::from([
                ("default_scope", vec![INCLUSION.to_owned()]),
                ("actor_type", vec![INCLUSION.to_owned()]),
            ]),
            "both refusals name the inclusion"
        );
    }

    /// The bio rule counts characters against the configured cap and
    /// answers the user endpoint's own message.
    #[test]
    fn the_note_check_mirrors_the_user_endpoint() {
        let config = Config::for_tests();
        let mut errors = register::FieldErrors::new();
        assert_eq!(
            checked_note(Some("hello"), &config, &mut errors).as_deref(),
            Some("hello"),
            "a short note waits for its rendering"
        );
        let long = "ü".repeat(config.max_bio_chars + 1);
        assert!(
            checked_note(Some(&long), &config, &mut errors).is_none(),
            "characters, not bytes"
        );
        assert_eq!(
            errors.get("bio"),
            Some(&vec![format!(
                "Bio is too long (maximum is {} characters)",
                config.max_bio_chars
            )]),
            "the user endpoint's wording, under bio"
        );
    }

    /// The settings-store reader: an object and the JSON text of one both
    /// come back as objects, junk text folds to `Value::Null`, and any
    /// other value is handed back for [`checked_store`]'s `else` arm to
    /// refuse — the fold and the let-else together gate every shape.
    #[test]
    fn the_store_reader_takes_objects_and_their_text() {
        assert_eq!(
            store_value(&json!({"a": 1})),
            json!({"a": 1}),
            "an object is its own answer"
        );
        assert_eq!(
            store_value(&json!("{\"a\": 1}")),
            json!({"a": 1}),
            "the text of one parses"
        );
        assert_eq!(
            store_value(&json!(["a"])),
            json!(["a"]),
            "an array is returned as it stands, for the object test to reject"
        );
        assert_eq!(
            store_value(&json!("junk")),
            Value::Null,
            "and junk text folds"
        );
    }
}
