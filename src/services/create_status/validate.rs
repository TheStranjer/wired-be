//! [`check_body`]: the body checks that need no database.
//!
//! The rules run in a fixed order — attachments (`super::validate_media`),
//! text and expiration (`super::validate_text`), poll
//! (`super::validate_poll`), then the content warning, the language, the
//! audience and the content type of this file — and the whole ruleset
//! answers a [`Checked`] of borrowed values or an
//! [`AppError::Unprocessable`] naming Pleroma's own wording.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::{
    CONTENT_TYPES, INVALID_CONTENT_TYPE, INVALID_VISIBILITY, MAX_LANGUAGE_CHARS, MAX_SPOILER_CHARS,
    params::{CheckedPoll, CreateStatusParams},
    validate_media::checked_media,
    validate_poll::checked_poll,
    validate_text::{check_text, checked_expiration},
};
use crate::{
    config::Config,
    error::{AppError, AppResult},
    models::status::{DEFAULT_CONTENT_TYPE, Visibility},
};

/// A checked status body: what [`check_body`] resolved, borrowed from the
/// [`CreateStatusParams`] it read, ready for a caller to build a row from.
#[derive(Debug)]
pub struct Checked<'a> {
    /// The post text, trimmed and within the instance's limit as
    /// [`crate::util::format::status_length`] measures it.
    pub text: &'a str,
    /// The content warning exactly as submitted, or the empty string when the
    /// body carried none; never trimmed, because a warning is stored verbatim.
    pub spoiler_text: &'a str,
    /// The submitted flag, or the author's default the caller passed in.
    pub sensitive: bool,
    /// The submitted audience, or the author's default scope the caller passed
    /// in, or [`Visibility::Public`] behind a stored scope that names nothing.
    pub visibility: Visibility,
    /// The submitted language tag; `None` when the body gave none or gave the
    /// empty string, which is how a client clears a tag it posted before.
    pub language: Option<&'a str>,
    /// The declared MIME type, always one of `CONTENT_TYPES` —
    /// [`DEFAULT_CONTENT_TYPE`] when the body did not say.
    pub content_type: &'static str,
    /// The moment the post deletes itself, `now` plus the submitted
    /// `expires_in`, or `None` when the body named no expiration — see
    /// `checked_expiration`.
    pub expires_at: Option<DateTime<Utc>>,
    /// The submitted `media_ids` read as ids, deduplicated to their first
    /// position and in the order the client sent them — empty when the body
    /// named none. Whether each one is a row the poster may actually claim
    /// is the write's decision, made atomically with the insert (see
    /// [`crate::models::status::insert`]).
    pub media_ids: Vec<Uuid>,
    /// The poll the body asked for, after [`check_body`]'s poll rules —
    /// see `checked_poll` — or `None` when the body carried none. The
    /// storage payload: both create arms hand it to
    /// [`crate::models::poll::create`] beside their status row, and the
    /// render batch reads it back into the entity's `poll` key.
    pub poll: Option<CheckedPoll>,
}

/// Checks a status body against the instance's limits and the author's two
/// defaults, or refuses it.
///
/// `config` supplies the length cap ([`Config::max_status_chars`],
/// `docs/ARCHITECTURE.md` decision 12). `default_scope` and
/// `default_sensitive` are the author's own preferences — Pleroma reads the
/// same two off the user as `default_scope` and `default_sensitive`, and here
/// they come in as plain values so that this module needs no database.
/// Whatever the body does not say, they answer; whatever it does say, they
/// lose to, except that a stored scope naming no [`Visibility`] falls back to
/// [`Visibility::Public`] rather than failing somebody's post.
///
/// The rules run in this order — the attachments (count, then id text), the
/// text (empty only with attachments or a poll standing behind it, then too
/// long), the poll (`checked_poll`, Pleroma's `CommonAPI` rules: options,
/// expiration, `media_ids`), the content warning, the language, the
/// visibility, the content type, then the expiration — so a body that
/// breaks several is refused for the first of them, and a media-only or
/// polled post is read before its empty text can be refused, the way
/// Pleroma's draft pipeline resolves attachments before it checks the
/// payload. The attachment refusal an id the poster may not claim earns is
/// not here — it is the write's, made on the database inside the status's
/// own transaction (see [`crate::models::status::insert`]).
///
/// An edit of a media-only or poll-only post reaches the same rules through
/// [`check_body_backed`], so its stored empty text is not refused.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `EMPTY_STATUS` (skipped when the body
/// carries attachments or a poll, either of which stands in for the text),
/// `OVER_LIMIT`, `INVALID_VISIBILITY` or `INVALID_CONTENT_TYPE`, with
/// `checked_media`'s refusals for more submitted `media_ids` than the
/// instance allows or for an entry that is no id at all, with
/// `checked_poll`'s six refusals for a poll that breaks one of its rules,
/// with Rails' too-long wording for a content warning or a language tag
/// past its cap, and with `checked_expiration`'s refusal for an
/// `expires_in` under the instance's floor or past the calendar.
pub fn check_body<'a>(
    config: &Config,
    default_scope: &str,
    default_sensitive: bool,
    params: &'a CreateStatusParams,
) -> AppResult<Checked<'a>> {
    check_body_backed(
        config,
        default_scope,
        default_sensitive,
        params,
        Backing::Body,
    )
}

/// What may stand in for the text of a body beyond what the body carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backing {
    /// Only the body's own `media_ids` and poll can stand in for the text:
    /// create, preview and schedule.
    Body,
    /// The stored post already carries an attachment or a poll that the
    /// body does not repeat: an edit of a media-only or poll-only post.
    StoredPost,
}

/// [`check_body`] with the caller's word on what else stands behind the text.
///
/// [`Backing::StoredPost`] lets an empty text pass the way an attachment or
/// a poll in the body would; [`Backing::Body`] is exactly [`check_body`].
///
/// # Errors
///
/// The refusals of [`check_body`].
pub fn check_body_backed<'a>(
    config: &Config,
    default_scope: &str,
    default_sensitive: bool,
    params: &'a CreateStatusParams,
    backing: Backing,
) -> AppResult<Checked<'a>> {
    let media_ids = checked_media(config, params)?;
    let has_poll = params.poll.is_some();
    let text = check_text(
        config,
        params.status.as_deref().unwrap_or_default(),
        backing == Backing::StoredPost || !media_ids.is_empty() || has_poll,
    )?;
    let poll = checked_poll(config, params)?;
    let spoiler_text = checked_spoiler(params.spoiler_text.as_deref())?;
    let language = checked_language(params.language.as_deref())?;
    let visibility = checked_visibility(params.visibility.as_deref(), default_scope)?;
    let content_type = checked_content_type(params.content_type.as_deref())?;
    let expires_at = checked_expiration(config, params.expires_in)?;
    Ok(Checked {
        text,
        spoiler_text,
        sensitive: params.sensitive.unwrap_or(default_sensitive),
        visibility,
        language,
        content_type,
        expires_at,
        media_ids,
        poll,
    })
}

/// The content warning: [`MAX_SPOILER_CHARS`] characters at most, and the
/// empty string when the body carried none — what the stored column holds for
/// a post without a warning.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with the too-long wording of [`checked_len`].
fn checked_spoiler(spoiler_text: Option<&str>) -> AppResult<&str> {
    checked_len(
        spoiler_text.unwrap_or_default(),
        MAX_SPOILER_CHARS,
        "Spoiler text",
    )
}

/// The language tag, or `None`: absent and empty both mean no tag, and a
/// present one must fit [`MAX_LANGUAGE_CHARS`] characters.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with the too-long wording of [`checked_len`].
fn checked_language(language: Option<&str>) -> AppResult<Option<&str>> {
    let Some(tag) = language.filter(|tag| !tag.is_empty()) else {
        return Ok(None);
    };
    checked_len(tag, MAX_LANGUAGE_CHARS, "Language").map(Some)
}

/// The audience: the submitted name, which must be one of the five
/// [`Visibility`] spellings; or, absent, the author's `default_scope` read the
/// same way, with [`Visibility::Public`] behind a stored value that names
/// nothing.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_VISIBILITY`] for a *submitted*
/// name outside the five; an unusable default is never the poster's fault.
fn checked_visibility(submitted: Option<&str>, default_scope: &str) -> AppResult<Visibility> {
    submitted.map_or_else(
        || Ok(default_scope.parse().unwrap_or(Visibility::Public)),
        |name| {
            name.parse()
                .ok()
                .ok_or_else(|| AppError::Unprocessable(INVALID_VISIBILITY.to_owned()))
        },
    )
}

/// The MIME type: the submitted one, which must equal an entry of
/// [`CONTENT_TYPES`] case-sensitively, or [`DEFAULT_CONTENT_TYPE`] when the
/// body did not name one.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_CONTENT_TYPE`] for anything
/// else, including Pleroma's own `text/x-markdown`.
fn checked_content_type(submitted: Option<&str>) -> AppResult<&'static str> {
    submitted.map_or(Ok(DEFAULT_CONTENT_TYPE), |name| {
        CONTENT_TYPES
            .into_iter()
            .find(|known| *known == name)
            .ok_or_else(|| AppError::Unprocessable(INVALID_CONTENT_TYPE.to_owned()))
    })
}

/// The text, when it fits `max_chars` scalar values (not bytes); otherwise
/// the 422 Rails puts in its changeset errors, naming `what` and the cap.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with
/// `Validation failed: {what} is too long (maximum is {max_chars} characters)`.
fn checked_len<'a>(text: &'a str, max_chars: usize, what: &str) -> AppResult<&'a str> {
    if text.chars().count() > max_chars {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: {what} is too long (maximum is {max_chars} characters)"
        )));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::services::create_status::{
        OVER_LIMIT, POLL_EXPIRATION_TOO_SOON, TOO_FEW_POLL_OPTIONS,
        test_support::{accepted, params, rejection},
    };
    #[test]
    fn a_plain_post_is_read_with_the_author_defaults() {
        let body = params(json!({"status": "  hi  "}));
        let checked = accepted(&body);
        assert_eq!(checked.text, "hi", "the text is trimmed");
        assert_eq!(checked.spoiler_text, "", "no warning means no text");
        assert!(!checked.sensitive, "absent takes the author's `false`");
        assert_eq!(checked.visibility, Visibility::Public, "and their scope");
        assert_eq!(checked.language, None, "and no language at all");
        assert_eq!(
            checked.content_type, DEFAULT_CONTENT_TYPE,
            "and the default content type"
        );
        assert!(
            CONTENT_TYPES.contains(&DEFAULT_CONTENT_TYPE),
            "which is one of the advertised types, so the fallback can never \
             be a value `checked_content_type` would refuse"
        );
    }

    #[test]
    fn the_author_defaults_answer_what_the_body_leaves_out() {
        let body = params(json!({"status": "hi"}));
        let checked = check_body(&Config::for_tests(), "local", true, &body)
            .unwrap_or_else(|err| panic!("an author default is always usable: {err}"));
        assert_eq!(
            checked.visibility,
            Visibility::Local,
            "Pleroma's extra scope comes from the author's row too"
        );
        assert!(checked.sensitive, "and so does the folded flag");
    }

    #[test]
    fn what_the_body_sends_wins_over_the_author_defaults() {
        let body = params(json!({
            "status": "hi",
            "visibility": "direct",
            "sensitive": false,
            "language": "en",
            "spoiler_text": "spoilers",
            "content_type": "text/html",
        }));
        // The defaults of `accepted` are `public` / not-sensitive, so every
        // one of these has to have come from the body.
        let checked = accepted(&body);
        assert_eq!(checked.visibility, Visibility::Direct, "submitted scope");
        assert!(!checked.sensitive, "a submitted `false` is not absent");
        assert_eq!(checked.language, Some("en"), "submitted tag");
        assert_eq!(checked.spoiler_text, "spoilers", "submitted warning");
        assert_eq!(checked.content_type, "text/html", "submitted type");
    }

    #[test]
    fn a_content_warning_is_capped_at_two_hundred_characters() {
        let edge = "x".repeat(MAX_SPOILER_CHARS);
        let body = params(json!({"status": "hi", "spoiler_text": edge}));
        assert_eq!(
            accepted(&body).spoiler_text,
            edge,
            "the cap itself fits, and is stored verbatim"
        );
        let padded = params(json!({"status": "hi", "spoiler_text": "  cats  "}));
        assert_eq!(
            accepted(&padded).spoiler_text,
            "  cats  ",
            "a warning is never trimmed, only the text is"
        );
        let over = params(json!({
            "status": "hi",
            "spoiler_text": "x".repeat(MAX_SPOILER_CHARS + 1),
        }));
        assert_eq!(
            rejection(&over),
            "Validation failed: Spoiler text is too long (maximum is 200 characters)",
            "one character past the cap is refused in Rails' own wording"
        );
    }

    #[test]
    fn a_language_is_capped_at_ten_characters_and_emptied_by_nothing() {
        let edge = params(json!({
            "status": "hi",
            "language": "a".repeat(MAX_LANGUAGE_CHARS),
        }));
        assert_eq!(
            accepted(&edge).language,
            Some("aaaaaaaaaa"),
            "the cap itself fits"
        );
        for value in [
            json!({"status": "hi", "language": ""}),
            json!({"status": "hi", "language": null}),
            json!({"status": "hi"}),
        ] {
            let body = params(value);
            assert_eq!(
                accepted(&body).language,
                None,
                "an empty, null or absent tag is no tag at all"
            );
        }
        let over = params(json!({
            "status": "hi",
            "language": "a".repeat(MAX_LANGUAGE_CHARS + 1),
        }));
        assert_eq!(
            rejection(&over),
            "Validation failed: Language is too long (maximum is 10 characters)",
            "eleven characters are one too many, in the same wording"
        );
    }

    #[test]
    fn the_two_caps_count_characters_not_bytes() {
        let warning = "…".repeat(MAX_SPOILER_CHARS);
        let body = params(json!({"status": "hi", "spoiler_text": warning}));
        assert_eq!(
            accepted(&body).spoiler_text,
            warning,
            "200 three-byte characters are 600 bytes and still fit"
        );
        let tag = "日本語言語タグ";
        let tagged = params(json!({"status": "hi", "language": tag}));
        assert_eq!(
            accepted(&tagged).language,
            Some(tag),
            "and so do 7 of them in a 10-character tag"
        );
    }

    #[test]
    fn every_known_visibility_name_is_accepted_as_itself() {
        for name in Visibility::ALL {
            let body = params(json!({
                "status": "hi",
                "visibility": name.as_str(),
            }));
            assert_eq!(
                accepted(&body).visibility,
                name,
                "`{name}` is its own audience, whatever the author defaults to"
            );
        }
    }

    #[test]
    fn an_unknown_submitted_visibility_is_refused() {
        for name in ["", "bogus", "Public", "published", " private"] {
            let body = params(json!({"status": "hi", "visibility": name}));
            assert_eq!(
                rejection(&body),
                INVALID_VISIBILITY,
                "`{name}` is not one of the five, and parsing them is \
                 case-sensitive"
            );
        }
    }

    #[test]
    fn an_unusable_stored_scope_falls_back_to_public() {
        for stored in ["", "bogus", "Public"] {
            let body = params(json!({"status": "hi"}));
            let checked = check_body(&Config::for_tests(), stored, false, &body)
                .unwrap_or_else(|err| panic!("a broken default is not the poster's fault: {err}"));
            assert_eq!(
                checked.visibility,
                Visibility::Public,
                "a row holding `{stored}` still posts publicly"
            );
        }
    }

    #[test]
    fn the_advertised_content_types_pass_and_every_other_is_refused() {
        for name in CONTENT_TYPES {
            let body = params(json!({"status": "hi", "content_type": name}));
            assert_eq!(
                accepted(&body).content_type,
                name,
                "`{name}` is one of the formats the instance advertises"
            );
        }
        let null = params(json!({"status": "hi", "content_type": null}));
        assert_eq!(
            accepted(&null).content_type,
            DEFAULT_CONTENT_TYPE,
            "a null type is as absent as no key"
        );
        for name in [
            "text/x-markdown",
            "TEXT/PLAIN",
            "text/plain ",
            "markdown",
            "",
        ] {
            let body = params(json!({"status": "hi", "content_type": name}));
            assert_eq!(
                rejection(&body),
                INVALID_CONTENT_TYPE,
                "`{name}` is not on the list: the match is exact and \
                 case-sensitive, so even Pleroma's own `text/x-markdown` \
                 alias is refused"
            );
        }
    }

    /// A poll carries an otherwise empty status: the empty refusal is
    /// skipped, the length cap is not lifted, and the poll rules run
    /// between the text and the visibility — a body breaking a poll rule
    /// and a scope rule is refused for the poll, and one breaking the
    /// poll's window and the top-level `expires_in` for the poll.
    #[test]
    fn a_poll_stands_in_for_the_text_and_runs_before_the_visibility() {
        let blank = params(json!({"status": "   ", "poll": {"options": ["a", "b"]}}));
        let checked = accepted(&blank);
        assert_eq!(checked.text, "", "the trimmed nothing is stored as nothing");
        assert!(checked.poll.is_some(), "and the question carries the post");

        let limit = Config::for_tests().max_status_chars;
        let over = params(json!({
            "status": "x".repeat(limit + 1),
            "poll": {"options": ["a", "b"]},
        }));
        assert_eq!(
            rejection(&over),
            OVER_LIMIT,
            "the poll does not lift the status length cap"
        );

        let bad_poll_and_bad_scope = params(json!({
            "status": "hi",
            "visibility": "sideways",
            "poll": {"options": ["only"]},
        }));
        assert_eq!(
            rejection(&bad_poll_and_bad_scope),
            TOO_FEW_POLL_OPTIONS,
            "the poll is refused before the visibility is looked at"
        );

        let bad_poll_and_bad_expiry = params(json!({
            "status": "",
            "poll": {"options": ["a", "b"], "expires_in": 1},
            "expires_in": -5,
        }));
        assert_eq!(
            rejection(&bad_poll_and_bad_expiry),
            POLL_EXPIRATION_TOO_SOON,
            "the poll's window answers before the top-level `checked_expiration` gets its turn"
        );
    }
}
