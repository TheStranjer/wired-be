//! The poll rules of [`check_body`]: `checked_poll` applies Pleroma's
//! `CommonAPI` checks to a [`CreateStatusParams`] carrying a [`PollRequest`].
//!
//! The option count, the option lengths, the expiration window and the
//! no-attachments cross-parameter rule — and the [`CheckedPoll`] both
//! create arms store beside their status row.
//!
//! [`check_body`]: crate::services::create_status::check_body
//! [`CheckedPoll`]: crate::services::create_status::CheckedPoll
//! [`CreateStatusParams`]: crate::services::create_status::CreateStatusParams
//! [`PollRequest`]: crate::services::create_status::PollRequest

use chrono::{DateTime, TimeDelta, Utc};

use super::{
    POLL_EXPIRATION_TOO_FAR, POLL_EXPIRATION_TOO_SOON, POLL_WITH_MEDIA, TOO_FEW_POLL_OPTIONS,
    params::{CheckedPoll, CreateStatusParams},
};
use crate::{
    config::Config,
    error::{AppError, AppResult},
};

/// The poll of a create body, checked by Pleroma's `CommonAPI` rules
/// (`validate_poll_options_amount`, `validate_poll_options_length` and
/// `validate_poll_expiration` of `lib/pleroma/web/common_api/utils.ex`),
/// or `None` when the body carried none.
///
/// In order: at least two options and at most
/// [`Config::max_poll_options`] of them (duplicates are allowed — Pleroma
/// dedups at render, and this implementation stores every option as
/// posted); then every option non-empty and no longer than
/// [`Config::max_poll_option_chars`] characters, measured in scalar
/// values; then the posted `expires_in`, when present, inside
/// [`Config::min_poll_expiration_secs`] to
/// [`Config::max_poll_expiration_secs`] — the accepted span becomes
/// [`CheckedPoll::expires_at`], `now` plus the seconds (a span so far out
/// no calendar can land on it closes never, which no configuration that
/// keeps the shipped ceiling can reach); and last the one cross-parameter
/// rule, that a poll sent beside non-empty `media_ids` is refused:
/// questions are not asked over attachments.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`TOO_FEW_POLL_OPTIONS`],
/// `Poll can't contain more than {max_poll_options} options`,
/// `Poll options cannot be longer than {max_poll_option_chars}
/// characters`, [`POLL_EXPIRATION_TOO_FAR`], [`POLL_EXPIRATION_TOO_SOON`]
/// or [`POLL_WITH_MEDIA`].
pub(super) fn checked_poll(
    config: &Config,
    params: &CreateStatusParams,
) -> AppResult<Option<CheckedPoll>> {
    let Some(request) = params.poll.as_ref() else {
        return Ok(None);
    };
    checked_poll_options(config, &request.options)?;
    checked_poll_option_chars(config, &request.options)?;
    let expires_at = checked_poll_expiration(config, request.expires_in)?;
    checked_poll_media(params.media_ids.as_deref())?;
    Ok(Some(CheckedPoll {
        options: request.options.clone(),
        expires_at,
        multiple: request.multiple.unwrap_or(false),
        hide_totals: request.hide_totals.unwrap_or(false),
    }))
}

/// The option-count arms of [`checked_poll`]: at least two, at most
/// [`Config::max_poll_options`].
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`TOO_FEW_POLL_OPTIONS`] or
/// `Poll can't contain more than {max_poll_options} options`.
fn checked_poll_options(config: &Config, options: &[String]) -> AppResult<()> {
    if options.len() < 2 {
        return Err(AppError::Unprocessable(TOO_FEW_POLL_OPTIONS.to_owned()));
    }
    if options.len() > config.max_poll_options {
        return Err(AppError::Unprocessable(format!(
            "Poll can't contain more than {} options",
            config.max_poll_options
        )));
    }
    Ok(())
}

/// The option-length arm of [`checked_poll`]: no option empty, none
/// past [`Config::max_poll_option_chars`] scalar values — Pleroma's one
/// message covers both spellings of a broken option.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `Poll options cannot be longer than
/// {max_poll_option_chars} characters`.
fn checked_poll_option_chars(config: &Config, options: &[String]) -> AppResult<()> {
    let broken = options.iter().any(|option| {
        let length = option.chars().count();
        length == 0 || length > config.max_poll_option_chars
    });
    if broken {
        return Err(AppError::Unprocessable(format!(
            "Poll options cannot be longer than {} characters",
            config.max_poll_option_chars
        )));
    }
    Ok(())
}

/// The expiration arm of [`checked_poll`]: the posted span, when present,
/// must land inside the instance's poll window, and the accepted instant
/// is computed once here — the same value [`create_status`] stores
/// as the poll's `expires_at` (see [`crate::models::poll::PollParams`]).
/// Bounds too wide for the `i64` the comparison runs in become the
/// nearest number it can hold: a ceiling that cannot be represented
/// refuses nothing, a floor that cannot be represented refuses every
/// submitted span, neither silently permissive.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`POLL_EXPIRATION_TOO_FAR`] above the
/// ceiling or [`POLL_EXPIRATION_TOO_SOON`] under the floor.
fn checked_poll_expiration(
    config: &Config,
    seconds: Option<i64>,
) -> AppResult<Option<DateTime<Utc>>> {
    let Some(secs) = seconds else {
        return Ok(None);
    };
    let max = i64::try_from(config.max_poll_expiration_secs).unwrap_or(i64::MAX);
    if secs > max {
        return Err(AppError::Unprocessable(POLL_EXPIRATION_TOO_FAR.to_owned()));
    }
    let min = i64::try_from(config.min_poll_expiration_secs).unwrap_or(i64::MAX);
    if secs < min {
        return Err(AppError::Unprocessable(POLL_EXPIRATION_TOO_SOON.to_owned()));
    }
    Ok(TimeDelta::try_seconds(secs).and_then(|span| Utc::now().checked_add_signed(span)))
}

/// The cross-parameter arm of [`checked_poll`]: a poll sent beside
/// non-empty `media_ids` is the attachment conflict; an empty or absent
/// list is no attachment at all.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`POLL_WITH_MEDIA`].
fn checked_poll_media(media_ids: Option<&[String]>) -> AppResult<()> {
    if media_ids.is_some_and(|ids| !ids.is_empty()) {
        return Err(AppError::Unprocessable(POLL_WITH_MEDIA.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        models::status::Visibility,
        services::create_status::{
            check_body,
            test_support::{accepted, params, rejection},
        },
    };
    /// `n` distinct option texts, numbered from zero.
    fn option_texts(n: usize) -> Vec<String> {
        (0..n).map(|index| format!("option {index}")).collect()
    }

    /// The reading of [`check_body`] under the stock test config with one
    /// field changed by `update`: the error's text, or `None` when the
    /// body was accepted.
    fn rejection_under<F>(config: &Config, body: &CreateStatusParams, update: F) -> Option<String>
    where
        F: FnOnce(&mut Config),
    {
        let mut owned = config.clone();
        update(&mut owned);
        check_body(&owned, "public", false, body)
            .map(|_| ())
            .map_err(|err| err.to_string())
            .err()
    }

    /// An accepted poll rides out of [`check_body`] as the [`CheckedPoll`]
    /// the create will store: options in order, the submitted
    /// span computed into a closing instant, and both flags as posted. A
    /// body without a poll leaves the field `None`; a `false` spelling is
    /// a decision, not an absence.
    #[test]
    fn an_accepted_poll_is_carried_in_full() {
        let absent = params(json!({"status": "hi"}));
        assert!(accepted(&absent).poll.is_none(), "no poll key, no poll");

        let body = params(json!({
            "status": "vote please",
            "poll": {
                "options": ["yes", "no"],
                "expires_in": 3600,
                "multiple": true,
                "hide_totals": true,
            },
        }));
        let poll = accepted(&body)
            .poll
            .expect("the checked body carries its poll");
        assert_eq!(poll.options, vec!["yes", "no"], "options keep their order");
        let due = poll
            .expires_at
            .unwrap_or_else(|| panic!("3600 seconds is a closing instant"));
        let span = due - Utc::now();
        assert!(
            (TimeDelta::seconds(3600) - span).num_seconds().abs() <= 1,
            "the poll closes about an hour out, not {span}"
        );
        assert!(poll.multiple, "the submitted any-choice flag");
        assert!(poll.hide_totals, "the submitted hidden-totals flag");

        let plain = params(json!({"status": "hi", "poll": {"options": ["a", "b"]}}));
        let plain = accepted(&plain).poll.expect("the bare poll is carried too");
        assert_eq!(
            plain.expires_at, None,
            "no expires_in is a poll that never closes"
        );
        assert!(!plain.multiple, "an absent flag is `false`, not undecided");
        assert!(!plain.hide_totals, "and so is the other one");

        let off = params(json!({
            "status": "hi",
            "poll": {"options": ["a", "b"], "multiple": false, "hide_totals": "off"},
        }));
        let off = accepted(&off)
            .poll
            .expect("a submitted `false` is as valid as a `true`");
        assert!(!off.multiple, "`false` stays false");
        assert!(!off.hide_totals, "and the form spelling `off` does too");
    }

    /// Fewer than two options — including a poll that named none at all,
    /// which the `serde(default)` keeps a service decision rather than an
    /// extractor error — is Pleroma's minimum refusal; two is the minimum
    /// met.
    #[test]
    fn a_poll_needs_at_least_two_options() {
        for poll in [
            json!({}),
            json!({"options": []}),
            json!({"options": ["only"]}),
        ] {
            let body = params(json!({"status": "hi", "poll": poll}));
            assert_eq!(
                rejection(&body),
                TOO_FEW_POLL_OPTIONS,
                "`{poll}` is less than a question"
            );
        }
        let body = params(json!({"status": "hi", "poll": {"options": ["a", "b"]}}));
        assert!(
            accepted(&body).poll.is_some(),
            "two options is the minimum met"
        );
    }

    /// The option ceiling at its exact boundary: twenty pass, twenty-one
    /// are refused, and the message names the configured number.
    #[test]
    fn a_poll_cannot_carry_more_options_than_the_ceiling() {
        let config = Config::for_tests();
        assert_eq!(
            config.max_poll_options, 20,
            "the stock ceiling is the twenty this test counts options against"
        );
        let exact = params(json!({"status": "hi", "poll": {"options": option_texts(20)}}));
        let checked = accepted(&exact)
            .poll
            .expect("twenty options are within the ceiling");
        assert_eq!(checked.options.len(), 20, "and all of them ride along");
        let over = params(json!({"status": "hi", "poll": {"options": option_texts(21)}}));
        assert_eq!(
            rejection(&over),
            "Poll can't contain more than 20 options",
            "one past the ceiling is refused, naming it"
        );
    }

    /// The ceiling is the configuration's, not a welded 20: an instance
    /// that raised it takes twenty-one, and one that lowered it names its
    /// own number in its own message.
    #[test]
    fn the_poll_option_ceiling_is_configurable() {
        let two = params(json!({"status": "hi", "poll": {"options": ["a", "b"]}}));
        let tight = rejection_under(&Config::for_tests(), &two, |config| {
            config.max_poll_options = 1;
        });
        assert_eq!(
            tight.expect("a ceiling of one refuses the two-option minimum"),
            "Poll can't contain more than 1 options",
            "the message names the configured number, not the shipped one"
        );
        let roomy = params(json!({"status": "hi", "poll": {"options": option_texts(21)}}));
        let opened = rejection_under(&Config::for_tests(), &roomy, |config| {
            config.max_poll_options = 21;
        });
        assert!(
            opened.is_none(),
            "the 21 the stock config refuses pass one that raised the ceiling to 21"
        );
    }

    /// The option-length rule at its exact boundaries: 200 characters pass,
    /// 201 are refused, and an empty option is refused with the same
    /// message — the one wording Pleroma's rule covers broken options
    /// with.
    #[test]
    fn a_poll_option_must_not_be_empty_or_over_long() {
        let exact = params(json!({
            "status": "hi",
            "poll": {"options": ["x".repeat(200), "b"]},
        }));
        assert!(
            accepted(&exact).poll.is_some(),
            "exactly 200 characters is still within the cap"
        );
        let over = params(json!({
            "status": "hi",
            "poll": {"options": ["a", "x".repeat(201)]},
        }));
        assert_eq!(
            rejection(&over),
            "Poll options cannot be longer than 200 characters",
            "one character past the cap is the refusal, naming it"
        );
        let empty = params(json!({"status": "hi", "poll": {"options": ["", "b"]}}));
        assert_eq!(
            rejection(&empty),
            "Poll options cannot be longer than 200 characters",
            "an empty option is refused with the same sentence"
        );
    }

    /// The length cap is the configuration's too: a short-capped instance
    /// measures and names its own number.
    #[test]
    fn the_poll_option_cap_is_configurable() {
        let body = params(json!({
            "status": "hi",
            "poll": {"options": ["0123456789", "eleven chars"]},
        }));
        let refused = rejection_under(&Config::for_tests(), &body, |config| {
            config.max_poll_option_chars = 10;
        });
        assert_eq!(
            refused.expect("the 11-character option breaks the configured cap"),
            "Poll options cannot be longer than 10 characters",
            "the message names the configured cap"
        );
    }

    /// The expiration window: 299 (and every shorter span, negatives
    /// included) are too soon, the 300 floor and the `31_536_000` ceiling
    /// pass, and `31_536_001` is too far — each with Pleroma's own words.
    #[test]
    fn the_poll_expiration_window_refuses_the_spans_it_cannot_take() {
        let posted = |secs: i64| {
            params(json!({
                "status": "hi",
                "poll": {"options": ["a", "b"], "expires_in": secs},
            }))
        };
        for short in [-1_i64, 0, 1, 299] {
            assert_eq!(
                rejection(&posted(short)),
                POLL_EXPIRATION_TOO_SOON,
                "{short} seconds is under the 300-second floor"
            );
        }
        assert!(
            accepted(&posted(300)).poll.is_some(),
            "the floor itself passes"
        );
        assert!(
            accepted(&posted(31_536_000)).poll.is_some(),
            "and the one-year ceiling itself passes"
        );
        assert_eq!(
            rejection(&posted(31_536_001)),
            POLL_EXPIRATION_TOO_FAR,
            "one second past the ceiling is too far"
        );
    }

    /// Both window bounds come from the configuration, and the stock
    /// window still refuses what an opened one takes.
    #[test]
    fn the_poll_expiration_window_is_configurable() {
        let short = params(json!({
            "status": "hi",
            "poll": {"options": ["a", "b"], "expires_in": 60},
        }));
        let lowered = rejection_under(&Config::for_tests(), &short, |config| {
            config.min_poll_expiration_secs = 30;
        });
        assert!(lowered.is_none(), "60 seconds pass a 30-second floor");
        assert_eq!(
            rejection(&short),
            POLL_EXPIRATION_TOO_SOON,
            "the stock 300-second floor still refuses it"
        );
        let far = params(json!({
            "status": "hi",
            "poll": {"options": ["a", "b"], "expires_in": 172_800},
        }));
        let shortened = rejection_under(&Config::for_tests(), &far, |config| {
            config.max_poll_expiration_secs = 86_400;
        });
        assert_eq!(
            shortened.expect("two days are past a one-day ceiling"),
            POLL_EXPIRATION_TOO_FAR,
            "the message is the same whatever the number was"
        );
    }

    /// A poll beside attachments is the conflict refusal; an empty
    /// `media_ids` list is no attachment at all, and `media_ids` without
    /// a poll is the attachments field — accepted here, claimed or
    /// refused by the write.
    #[test]
    fn a_poll_cannot_have_attachments() {
        let with_media = params(json!({
            "status": "hi",
            "poll": {"options": ["a", "b"]},
            "media_ids": ["01934567-89ab-7cde-f012-3456789abcde"],
        }));
        assert_eq!(
            rejection(&with_media),
            POLL_WITH_MEDIA,
            "a question over a picture is refused"
        );
        let empty_list = params(json!({
            "status": "hi",
            "poll": {"options": ["a", "b"]},
            "media_ids": [],
        }));
        assert!(
            accepted(&empty_list).poll.is_some(),
            "an empty id list names no attachment, so there is no conflict"
        );
        let media_only = params(json!({
            "status": "hi",
            "media_ids": ["01934567-89ab-7cde-f012-3456789abcde"],
        }));
        let checked = accepted(&media_only);
        assert!(
            checked.poll.is_none(),
            "media_ids without a poll is no conflict — just the attachment ids"
        );
    }

    /// A `direct` poll is a private question, which Pleroma posts: the
    /// audience check neither sees nor refuses the poll.
    #[test]
    fn a_direct_poll_is_allowed() {
        let body = params(json!({
            "status": "vote quietly",
            "visibility": "direct",
            "poll": {"options": ["a", "b"]},
        }));
        let checked = accepted(&body);
        assert_eq!(
            checked.visibility,
            Visibility::Direct,
            "the audience posted"
        );
        assert!(checked.poll.is_some(), "and the poll carried");
    }

    /// Repeated options are the poster's business: the count bounds are
    /// kept, the texts are not deduplicated.
    #[test]
    fn duplicate_poll_options_are_allowed() {
        let body = params(json!({"status": "hi", "poll": {"options": ["same", "same"]}}));
        let poll = accepted(&body).poll.expect("duplicates are no rule broken");
        assert_eq!(
            poll.options,
            vec!["same", "same"],
            "both copies ride along as posted"
        );
    }
}
