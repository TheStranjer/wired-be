//! The Pleroma-side merges of `update_credentials`: the birthday, the
//! `settings_store` blob and the `actor_type`/`bot` pair, each the
//! submitted value over what the row holds.
//! [`profile_fields`](super::credentials_merge::profile_fields) calls
//! them as part of the one fold.
//!
//! [`settings_store`]: crate::models::user::User::settings_store
//!
//! The ten Pleroma preference flags — `hide_followers`, `hide_follows`,
//! `hide_followers_count`, `hide_follows_count`, `hide_favorites`,
//! `show_role`, `allow_following_move`,
//! `accepts_chat_messages`, `no_rich_text` and `show_birthday` — are
//! [`flex::opt_bool`] booleans like the three Mastodon ones.
//! `skip_thread_containment` is not among them: it is admin-only and any
//! value sent here is a 422. `birthday` is
//! `YYYY-MM-DD` text (the empty string clears it, anything else that is not a
//! date is 422, and a date after today is 422 as Pleroma's age check refuses
//! it). `pleroma_settings_store` is a JSON object — the JSON *text* of one
//! from a form body — merged key by key into the stored `settings_store` and
//! refused with the 422 of
//! [`STORE_TOO_LARGE`](crate::api::pleroma_settings::STORE_TOO_LARGE) when
//! the merged blob passes 1 MiB, the cap
//! `PATCH /api/v1/pleroma/settings/{app}` measures with (see
//! [`check_store_size`]). `actor_type` is `Person` or `Service` (anything
//! else, `Group` included, is 422); it picks the actor kind and the `bot`
//! flag follows it, while `bot` alone picks the kind Pleroma's controller
//! does — `true` a `Service`, `false` a `Person` — so the two columns never
//! disagree.
//!
//! The answer
//! is a 200 [`Account::for_owner`] render — the owner view with its
//! `source` block, the same one `verify_credentials` answers — and every
//! accepted request fires `AccountEvent::ProfileChanged`, which
//! `ActivityPub` answers with the actor's `Update`.
//!

use chrono::NaiveDate;
use serde_json::{Map, Value};

use crate::{
    api::pleroma_settings::{check_store_size, shallow_merge},
    error::{AppError, AppResult},
    models::user::{Flag, User},
};

use super::{ACTOR_TYPES, PERSON_ACTOR_TYPE, SERVICE_ACTOR_TYPE, credentials::ProfileParams};

/// The 422 body for an `actor_type` outside [`ACTOR_TYPES`].
const ACTOR_TYPE_INVALID: &str = "actor_type: is not included in the list";

/// The 422 body for a `pleroma_settings_store` that is neither a JSON object
/// nor the JSON text of one.
const STORE_NOT_OBJECT: &str = "pleroma_settings_store: expected a JSON object";

/// The 422 body for a `birthday` that is not `YYYY-MM-DD` text.
const BIRTHDAY_INVALID: &str = "birthday: is not a valid date";

/// The 422 body for a `birthday` after today: Pleroma's `validate_min_age`
/// reduced to the one rule this ticket states.
const BIRTHDAY_FUTURE: &str = "birthday: must not be in the future";

/// The format of a `birthday` parameter: an ISO 8601 calendar date, the
/// `YYYY-MM-DD` text this ticket fixes.
const BIRTHDAY_FORMAT: &str = "%Y-%m-%d";

/// The `birthday` to store: the parsed `submitted` date, the empty string
/// clearing the column and `None` (no parameter) keeping the stored one.
/// Text that is not the `YYYY-MM-DD` spelling is [`BIRTHDAY_INVALID`] —
/// chrono's parser also takes `1990-1-3`, so the parsed date has to print
/// back exactly what was sent — and a date after `today` is
/// [`BIRTHDAY_FUTURE`], Pleroma's `validate_min_age` kept to the one rule
/// this ticket states.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with those two messages.
pub(super) fn merged_birthday(
    submitted: Option<&str>,
    stored: Option<NaiveDate>,
    today: NaiveDate,
) -> AppResult<Option<NaiveDate>> {
    let Some(raw) = submitted else {
        return Ok(stored);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    let parsed = NaiveDate::parse_from_str(raw, BIRTHDAY_FORMAT)
        .map_err(|_err| AppError::Unprocessable(BIRTHDAY_INVALID.to_owned()))?;
    if parsed.format(BIRTHDAY_FORMAT).to_string() != raw {
        return Err(AppError::Unprocessable(BIRTHDAY_INVALID.to_owned()));
    }
    if parsed > today {
        return Err(AppError::Unprocessable(BIRTHDAY_FUTURE.to_owned()));
    }
    Ok(Some(parsed))
}

/// The `settings_store` to write: the stored blob with the submitted
/// `pleroma_settings_store` merged into it key by key ([`shallow_merge`],
/// Pleroma's `Map.merge` in `User.update_changeset` — a submitted key wins,
/// every other stored entry stays) and then measured by
/// [`check_store_size`]. No parameter keeps the stored blob; a merged blob
/// over the cap is refused here, before the write, so nothing changes.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`STORE_NOT_OBJECT`] for a value that is
/// neither a JSON object nor the JSON text of one, or with
/// [`STORE_TOO_LARGE`](crate::api::pleroma_settings::STORE_TOO_LARGE) for a
/// store over the limit.
pub(super) fn merged_store(submitted: Option<&Value>, stored: &Value) -> AppResult<Value> {
    let Some(patch) = submitted else {
        return Ok(stored.clone());
    };
    let merged = shallow_merge(stored, &store_object(patch)?);
    check_store_size(&merged)?;
    Ok(merged)
}

/// The object a submitted `pleroma_settings_store` names: a JSON body
/// carries the object itself, a form or multipart body carries its JSON
/// *text*, which is parsed here; anything else — an array, a scalar, text
/// that is no JSON — has no keys to merge and is the 422.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`STORE_NOT_OBJECT`].
fn store_object(submitted: &Value) -> AppResult<Map<String, Value>> {
    let decoded = match submitted {
        Value::String(text) => serde_json::from_str::<Value>(text)
            .map_err(|_err| AppError::Unprocessable(STORE_NOT_OBJECT.to_owned())),
        Value::Object(map) => Ok(Value::Object(map.clone())),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) => {
            Err(AppError::Unprocessable(STORE_NOT_OBJECT.to_owned()))
        }
    }?;
    let Value::Object(map) = decoded else {
        return Err(AppError::Unprocessable(STORE_NOT_OBJECT.to_owned()));
    };
    Ok(map)
}

/// The `actor_type` and `bot` pair after this body, in Pleroma's order: the
/// `bot` parameter picks the actor kind ([`actor_type_for_bot`]) and an
/// explicit `actor_type` wins over it and re-decides `bot` from the kind, so
/// the two columns never disagree. A body carrying neither leaves both
/// stored values as they are.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`ACTOR_TYPE_INVALID`] for a kind
/// outside [`ACTOR_TYPES`] (`Group` and every other actor type is not
/// settable through this endpoint).
pub(super) fn merged_actor(params: &ProfileParams, current: &User) -> AppResult<(String, Flag)> {
    match params.actor_type.as_deref() {
        Some(kind) if ACTOR_TYPES.contains(&kind) => {
            Ok((kind.to_owned(), Flag::from(kind == SERVICE_ACTOR_TYPE)))
        }
        Some(_) => Err(AppError::Unprocessable(ACTOR_TYPE_INVALID.to_owned())),
        None => Ok(params.bot.map_or_else(
            || (current.actor_type.clone(), current.bot),
            |automated| {
                (
                    actor_type_for_bot(automated).to_owned(),
                    Flag::new(automated),
                )
            },
        )),
    }
}

/// The `actor_type` Pleroma stores for an account flagged as a bot (or not):
/// an automated account is a `Service`, a human one a [`Person`].
///
/// [`Person`]: crate::models::user::DEFAULT_ACTOR_TYPE
#[must_use]
const fn actor_type_for_bot(automated: bool) -> &'static str {
    if automated {
        SERVICE_ACTOR_TYPE
    } else {
        PERSON_ACTOR_TYPE
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{
        api::accounts::test_support::{params, stored_user},
        models::user::DEFAULT_ACTOR_TYPE,
    };

    use super::*;

    /// The same day every `merged_birthday` test measures "the future"
    /// against, so the assertions do not drift with the clock.
    const TODAY: &str = "2026-02-28";

    /// `TODAY` as the date [`merged_birthday`] compares against.
    fn today() -> NaiveDate {
        NaiveDate::parse_from_str(TODAY, BIRTHDAY_FORMAT).expect("the fixture date is valid")
    }

    #[test]
    fn a_birthday_is_a_past_date_or_the_clear_or_the_422() {
        let stored = today().pred_opt().expect("yesterday exists");
        assert_eq!(
            merged_birthday(Some(TODAY), Some(stored), today()).unwrap(),
            Some(today()),
            "today is not in the future"
        );
        assert_eq!(
            merged_birthday(Some("1990-01-31"), Some(stored), today()).unwrap(),
            NaiveDate::from_ymd_opt(1990, 1, 31),
            "a submitted date replaces the stored one"
        );
        assert_eq!(
            merged_birthday(None, Some(stored), today()).unwrap(),
            Some(stored),
            "no parameter keeps the stored date"
        );
        assert_eq!(
            merged_birthday(Some(""), Some(stored), today()).unwrap(),
            None,
            "the empty string clears it"
        );
        for text in ["1990-1-31", "31-01-1990", "1990-13-01", "yesterday"] {
            let err = merged_birthday(Some(text), None, today()).expect_err("not a birthday");
            assert_eq!(err.to_string(), BIRTHDAY_INVALID, "the parse message");
        }
        for text in ["2026-03-01", "2099-01-01"] {
            let err = merged_birthday(Some(text), None, today()).expect_err("a future birthday");
            assert_eq!(err.to_string(), BIRTHDAY_FUTURE, "the future message");
        }
    }

    #[test]
    fn the_settings_store_merges_key_by_key() {
        let stored = stored_user();
        let merged = merged_store(
            Some(&json!({"locale": "de", "theme": null})),
            &stored.settings_store,
        )
        .expect("a small object fits");
        assert_eq!(
            merged,
            json!({"locale": "de", "theme": Value::Null}),
            "submitted keys replace, a null is stored as null, and only the merged keys change"
        );
        assert_eq!(
            merged_store(None, &stored.settings_store).unwrap(),
            stored.settings_store,
            "no parameter keeps the stored blob"
        );
        let from_text = merged_store(
            Some(&json!(r#"{"custom": {"nested": 1}}"#)),
            &stored.settings_store,
        )
        .expect("a form carries the JSON text of the object");
        assert_eq!(
            from_text["custom"],
            json!({"nested": 1}),
            "the text parses to the object it names"
        );
    }

    #[test]
    fn a_settings_store_submission_is_an_object_within_the_cap() {
        use crate::api::pleroma_settings::{MAX_SETTINGS_STORE_BYTES, STORE_TOO_LARGE};

        let stored = stored_user().settings_store;
        for patch in [json!(5), json!("plain text"), json!([1, 2]), json!(true)] {
            let err = merged_store(Some(&patch), &stored).expect_err("a value with no keys");
            assert_eq!(err.to_string(), STORE_NOT_OBJECT, "the object message");
        }
        let blob = "x".repeat(MAX_SETTINGS_STORE_BYTES + 1);
        let err = merged_store(Some(&json!({"blob": blob})), &stored)
            .expect_err("over one mebibyte does not fit");
        assert_eq!(err.to_string(), STORE_TOO_LARGE, "the size message");
        assert!(
            matches!(err, AppError::Unprocessable(_)),
            "both store refusals are 422s"
        );
    }

    /// `merged_actor` over the stored fixture row, as the `(actor_type,
    /// bot)` pair the body leaves.
    fn actor_pair(body: &str) -> (String, bool) {
        let (kind, bot) =
            merged_actor(&params(body), &stored_user()).expect("the fixture body is accepted");
        (kind, bot.get())
    }

    #[test]
    fn actor_type_and_bot_are_written_as_one_pair() {
        assert_eq!(
            actor_pair(r#"{"actor_type": "Service"}"#),
            ("Service".to_owned(), true),
            "the stated kind is stored and `bot` follows it"
        );
        assert_eq!(
            actor_pair(r#"{"actor_type": "Person"}"#),
            ("Person".to_owned(), false),
            "and so the other way round"
        );
        assert_eq!(
            actor_pair(r#"{"bot": true}"#),
            ("Service".to_owned(), true),
            "`bot=true` picks the actor kind Pleroma stores for a bot"
        );
        assert_eq!(
            actor_pair(r#"{"bot": false}"#),
            ("Person".to_owned(), false),
            "and `bot=false` picks `Person`"
        );
        assert_eq!(
            actor_pair(r#"{"bot": false, "actor_type": "Service"}"#),
            ("Service".to_owned(), true),
            "an explicit `actor_type` wins, and `bot` reads back from the kind"
        );
        assert_eq!(
            actor_pair("{}"),
            (DEFAULT_ACTOR_TYPE.to_owned(), false),
            "a body with neither parameter keeps both stored columns"
        );
        for text in ["Group", "person", ""] {
            let body = params(&format!(r#"{{"actor_type": "{text}"}}"#));
            let err = merged_actor(&body, &stored_user()).expect_err("no such actor kind");
            assert_eq!(err.to_string(), ACTOR_TYPE_INVALID, "the kind message");
        }
    }
}
