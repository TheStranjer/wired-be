//! What `POST /api/v1/statuses` accepts: a request body as a client sends it,
//! checked and resolved into the values a status is built from.
//!
//! The reference is Pleroma's `StatusController.create`
//! (`lib/pleroma/web/mastodon_api/controllers/status_controller.ex`) and the
//! one limit check it reaches through `CommonAPI.post`,
//! `Utils.validate_character_limit` (`lib/pleroma/web/common_api/utils.ex`):
//! trimmed text that is empty is refused with "Cannot post an empty status
//! without attachments", text past the instance's limit with "The status is
//! over the character limit", and every parameter the body leaves out falls
//! back to a stored or configured default instead of being an error.
//!
//! Two differences from Pleroma, both deliberate. The measure is
//! [`status_length`], which charges each bare URL `URL_WEIGHT` characters the
//! way Mastodon's `charactersReservedPerUrl` does where Pleroma counts the raw
//! text; and the author's two defaults arrive as plain arguments to
//! [`check_body`] rather than being read off a row, so nothing here touches a
//! database and every rule is testable without one. The route that supplies
//! those two values from the caller's row and stores what comes back is a
//! later ticket (decision 15), as are the endpoint's remaining parameters:
//! `media_ids`, `poll`, `in_reply_to_id` and `scheduled_at` join
//! [`CreateStatusParams`] as extra `Option`s there, and until then a body that
//! sends them is only a body whose known fields are checked here.
//!
//! Every refusal is an [`AppError::Unprocessable`] — a 422 whose body is
//! `{"error":"…"}` (decision 8) — in Pleroma's or Mastodon's own wording,
//! pinned by the tests below: the two limit messages, `Invalid visibility` for
//! a submitted scope outside the five [`Visibility`] names, `Invalid
//! content_type` for a MIME type outside `CONTENT_TYPES`, and Rails'
//! too-long sentence for a content warning or a language tag past its cap.

use serde::Deserialize;

use crate::{
    config::Config,
    error::{AppError, AppResult},
    models::status::{DEFAULT_CONTENT_TYPE, Visibility},
    util::format::status_length,
};

/// What one bare URL costs in a status: Mastodon's
/// `charactersReservedPerUrl`, passed to [`status_length`].
const URL_WEIGHT: usize = 23;

/// Longest content warning the endpoint accepts, in characters.
const MAX_SPOILER_CHARS: usize = 200;

/// Longest language tag the endpoint accepts, in characters.
const MAX_LANGUAGE_CHARS: usize = 10;

/// The MIME types a status may declare, matched verbatim and
/// case-sensitively — the list the instance and nodeinfo endpoints advertise
/// as this server's `post_formats`.
const CONTENT_TYPES: [&str; 3] = ["text/plain", "text/html", "text/markdown"];

/// 422 body for text that leaves nothing behind once trimmed: Pleroma's
/// `validate_character_limit` wording, which an attachments-only post will be
/// allowed to send once a later ticket adds `media_ids`.
const EMPTY_STATUS: &str = "Cannot post an empty status without attachments";

/// 422 body for text over the instance's limit (Pleroma's wording again).
const OVER_LIMIT: &str = "The status is over the character limit";

/// 422 body for a submitted visibility outside the five names, Pleroma's
/// message for its `:visibility_error`.
const INVALID_VISIBILITY: &str = "Invalid visibility";

/// 422 body for a submitted MIME type outside [`CONTENT_TYPES`].
const INVALID_CONTENT_TYPE: &str = "Invalid content_type";

/// The body of `POST /api/v1/statuses`, as the client wrote it.
///
/// Every field is an `Option` with a serde `default`, so a key the body omits
/// or sends as `null` stays this module's decision instead of becoming a
/// deserialization error; a later ticket joins the struct with `media_ids`,
/// `poll`, `in_reply_to_id` and `scheduled_at` as further `Option`s, the same
/// way. The `sensitive` field is a [`flex`] boolean, so a form body's
/// `sensitive=on` spells what a JSON body's `true` spells.
///
/// [`flex`]: crate::extract::flex
#[derive(Debug, Default, Deserialize)]
pub struct CreateStatusParams {
    /// The post text; trimmed, and required to leave something behind.
    #[serde(default)]
    status: Option<String>,
    /// The content warning shown in place of the post; at most
    /// `MAX_SPOILER_CHARS` characters, and the empty string when absent.
    #[serde(default)]
    spoiler_text: Option<String>,
    /// Whether the post starts folded; the author's own default when the body
    /// does not say.
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    sensitive: Option<bool>,
    /// The requested audience, one of the five [`Visibility`] names; the
    /// author's default scope when absent.
    #[serde(default)]
    visibility: Option<String>,
    /// The content language tag; at most `MAX_LANGUAGE_CHARS` characters,
    /// and no tag at all when absent or empty.
    #[serde(default)]
    language: Option<String>,
    /// The MIME type of the `status` text; [`DEFAULT_CONTENT_TYPE`] when
    /// absent.
    #[serde(default)]
    content_type: Option<String>,
}

/// A checked status body: what [`check_body`] resolved, borrowed from the
/// [`CreateStatusParams`] it read, ready for a caller to build a row from.
#[derive(Debug)]
pub struct Checked<'a> {
    /// The post text, trimmed and within the instance's limit as
    /// [`status_length`] measures it.
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
/// The rules run in this order — the text (empty, then too long), the content
/// warning, the language, the visibility, then the content type — so a body
/// that breaks several is refused for the first of them.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `EMPTY_STATUS`, `OVER_LIMIT`,
/// `INVALID_VISIBILITY` or `INVALID_CONTENT_TYPE`, or with Rails' too-long
/// wording for a content warning or a language tag past its cap.
pub fn check_body<'a>(
    config: &Config,
    default_scope: &str,
    default_sensitive: bool,
    params: &'a CreateStatusParams,
) -> AppResult<Checked<'a>> {
    let text = check_text(config, params.status.as_deref().unwrap_or_default())?;
    let spoiler_text = checked_spoiler(params.spoiler_text.as_deref())?;
    let language = checked_language(params.language.as_deref())?;
    let visibility = checked_visibility(params.visibility.as_deref(), default_scope)?;
    let content_type = checked_content_type(params.content_type.as_deref())?;
    Ok(Checked {
        text,
        spoiler_text,
        sensitive: params.sensitive.unwrap_or(default_sensitive),
        visibility,
        language,
        content_type,
    })
}

/// The post text: trimmed, non-empty, and no longer than
/// [`Config::max_status_chars`] measured with [`status_length`]'s weighting.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`EMPTY_STATUS`] or [`OVER_LIMIT`].
fn check_text<'a>(config: &Config, text: &'a str) -> AppResult<&'a str> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(AppError::Unprocessable(EMPTY_STATUS.to_owned()));
    }
    if status_length(trimmed, URL_WEIGHT) > config.max_status_chars {
        return Err(AppError::Unprocessable(OVER_LIMIT.to_owned()));
    }
    Ok(trimmed)
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
    use serde_json::{Value, json};

    use super::*;

    /// The parameters a JSON body deserializes to: the same
    /// `into_params::<CreateStatusParams>` call an [`ApiBody`] makes, so these
    /// tests see a body exactly as the endpoint will.
    ///
    /// [`ApiBody`]: crate::extract::ApiBody
    fn params(value: Value) -> CreateStatusParams {
        crate::extract::into_params::<CreateStatusParams>(value)
            .expect("every body in these tests fits the parameter type")
    }

    /// The reading of `body` by [`check_body`] under the stock test
    /// configuration and an author who defaults to `public` and to not
    /// marking posts sensitive.
    fn accepted(body: &CreateStatusParams) -> Checked<'_> {
        check_body(&Config::for_tests(), "public", false, body)
            .unwrap_or_else(|err| panic!("this body should be accepted: {err}"))
    }

    /// The 422 message [`check_body`] answers for `body` under the same
    /// configuration and defaults.
    fn rejection(body: &CreateStatusParams) -> String {
        match check_body(&Config::for_tests(), "public", false, body) {
            Err(err) => err.to_string(),
            Ok(checked) => panic!("this body should be refused, got `{}`", checked.text),
        }
    }

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
    fn a_sensitive_flag_accepts_the_form_spellings() {
        let on = params(json!({"status": "hi", "sensitive": "on"}));
        assert!(accepted(&on).sensitive, "`\"on\"` means true");
        let zero = params(json!({"status": "hi", "sensitive": "0"}));
        assert!(!accepted(&zero).sensitive, "`\"0\"` means false");
        let null = params(json!({"status": "hi", "sensitive": null}));
        assert!(!accepted(&null).sensitive, "a null is as absent as no key");
    }

    #[test]
    fn a_status_with_nothing_to_say_is_refused() {
        for value in [
            json!({}),
            json!({"status": null}),
            json!({"status": ""}),
            json!({"status": "   \n\t "}),
        ] {
            let body = params(value);
            assert_eq!(
                rejection(&body),
                EMPTY_STATUS,
                "a status with no text and no attachments is Pleroma's message"
            );
        }
    }

    #[test]
    fn a_status_over_the_character_limit_is_refused() {
        let limit = Config::for_tests().max_status_chars;
        let exact = params(json!({"status": "x".repeat(limit)}));
        assert_eq!(
            accepted(&exact).text.chars().count(),
            limit,
            "exactly the limit is still within it"
        );
        let over = params(json!({"status": "x".repeat(limit + 1)}));
        assert_eq!(
            rejection(&over),
            OVER_LIMIT,
            "and one character more is the over-limit message"
        );
    }

    #[test]
    fn the_limit_counts_a_bare_url_as_twenty_three_characters() {
        // 49 characters as written, 23 as a client counts them: over a
        // 40-character limit by length, under it by weighting.
        let url = "https://example.com/a/very/long/path?query=string";
        assert_eq!(
            status_length(url, URL_WEIGHT),
            URL_WEIGHT,
            "one bare URL costs exactly `URL_WEIGHT`, which is what this \
             module pins as Mastodon's `charactersReservedPerUrl`"
        );
        let mut config = Config::for_tests();
        config.max_status_chars = 40;
        let body = params(json!({"status": url}));
        let checked = check_body(&config, "public", false, &body)
            .unwrap_or_else(|_| panic!("a 49-character URL weighs {URL_WEIGHT} here"));
        assert_eq!(checked.text, url, "and the URL is stored verbatim");

        let plain = params(json!({"status": "x".repeat(41)}));
        let refused = check_body(&config, "public", false, &plain)
            .err()
            .unwrap_or_else(|| panic!("41 plain characters do not fit a 40-character limit"));
        assert_eq!(
            refused.to_string(),
            OVER_LIMIT,
            "which is the same refusal the weighting saves the URL from"
        );
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
}
