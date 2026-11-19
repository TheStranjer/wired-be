//! The text rule of [`check_body`]: `check_text` trims the post and caps
//! what is left, refusing empty text that stands alone.
//!
//! The cap is [`Config::max_status_chars`] as [`status_length`] weighs it,
//! each bare URL costing `URL_WEIGHT` characters. The instance's expiration
//! arms live here too: `expiration_floor` widens the configured minimum
//! into the comparison's `i64`, and `checked_expiration` turns an accepted
//! `expires_in` into the instant it counts out from now.
//!
//! [`check_body`]: crate::services::create_status::check_body

use chrono::{DateTime, TimeDelta, Utc};

use super::{EMPTY_STATUS, EXPIRATION_TOO_LARGE, OVER_LIMIT};
use crate::{
    config::Config,
    error::{AppError, AppResult},
    util::format::status_length,
};

/// What one bare URL costs in a status: Mastodon's
/// `charactersReservedPerUrl`, passed to [`status_length`].
const URL_WEIGHT: usize = 23;

/// The post text: trimmed, no longer than [`Config::max_status_chars`]
/// measured with [`status_length`]'s weighting, and — with neither
/// attachments nor a poll standing beside it — not empty. A media-only
/// post trims to nothing and still stands, and so does a polled one:
/// Pleroma's `validate_character_limit` refuses `""` only over an empty
/// attachment list, and its `CommonAPI` posts a question with no text.
/// The cap itself survives either companion; a poll does not lift it.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`EMPTY_STATUS`] (empty text and
/// `can_stand_empty` false) or [`OVER_LIMIT`].
pub(super) fn check_text<'a>(
    config: &Config,
    text: &'a str,
    can_stand_empty: bool,
) -> AppResult<&'a str> {
    let trimmed = text.trim();
    if trimmed.is_empty() && !can_stand_empty {
        return Err(AppError::Unprocessable(EMPTY_STATUS.to_owned()));
    }
    if status_length(trimmed, URL_WEIGHT) > config.max_status_chars {
        return Err(AppError::Unprocessable(OVER_LIMIT.to_owned()));
    }
    Ok(trimmed)
}

/// The instance's expiration floor as the `i64` the `expires_in` comparison
/// runs in: [`min_expiration_secs`](crate::config::Config::min_expiration_secs)
/// widened, with a floor too wide for `i64` becoming the largest number the
/// comparison can hold — a floor nobody can post past, not a silent zero.
fn expiration_floor(config: &Config) -> i64 {
    i64::try_from(config.min_expiration_secs).unwrap_or(i64::MAX)
}

/// The expiry an `expires_in` of `seconds` describes: `None` when the body
/// named none, and otherwise the instant `seconds` from now — Pleroma's
/// `:expiration, minimum_expiration_interval` rule, which here is
/// [`Config::min_expiration_secs`]: a shorter span (zero and negatives
/// included) is refused rather than silently lengthened, and so is one whose
/// span no calendar can land on. The accepted instant is computed once,
/// here, and travels to the stored row and to the scheduled delete's
/// `run_at` as the same value.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `expires_in must be at least {floor}
/// seconds` — naming this instance's floor — for a submitted value under
/// [`expiration_floor`], and with [`EXPIRATION_TOO_LARGE`] for a span that
/// does not fit a timestamp.
pub(super) fn checked_expiration(
    config: &Config,
    seconds: Option<i64>,
) -> AppResult<Option<DateTime<Utc>>> {
    let Some(secs) = seconds else {
        return Ok(None);
    };
    let floor = expiration_floor(config);
    if secs < floor {
        return Err(AppError::Unprocessable(format!(
            "expires_in must be at least {floor} seconds"
        )));
    }
    TimeDelta::try_seconds(secs)
        .and_then(|span| Utc::now().checked_add_signed(span))
        .map(Some)
        .ok_or_else(|| AppError::Unprocessable(EXPIRATION_TOO_LARGE.to_owned()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::services::create_status::{
        check_body,
        test_support::{accepted, params, rejection},
    };
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

    /// Pleroma's `expires_in`: the accepted span becomes the exact instant
    /// it counts out from now, the floor itself passes, and no expiry at
    /// all — absent or null — is no `expires_at`.
    #[test]
    fn an_expires_in_becomes_the_instant_it_counts_out() {
        let absent = params(json!({"status": "hi"}));
        assert_eq!(accepted(&absent).expires_at, None, "no key, no expiry");
        let null = params(json!({"status": "hi", "expires_in": null}));
        assert_eq!(accepted(&null).expires_at, None, "and null is as absent");

        for secs in [600_i64, 3_600, 86_400] {
            let before = Utc::now();
            let body = params(json!({"status": "hi", "expires_in": secs}));
            let due = accepted(&body)
                .expires_at
                .unwrap_or_else(|| panic!("{secs} seconds is an expiry"));
            let span = due - before;
            assert!(
                (TimeDelta::seconds(secs) - span).num_seconds().abs() <= 1,
                "{secs} seconds lands within a second of the span it names"
            );
        }
    }

    /// Under the floor, over the calendar: the two 422s `expires_in` owes,
    /// in the wording the ticket pins for the first and naming the value
    /// from the configuration.
    #[test]
    fn the_floor_and_the_calendar_refuse_the_expirations_they_cannot_take() {
        for secs in [-1_i64, 0, 1, 599] {
            let body = params(json!({"status": "hi", "expires_in": secs}));
            assert_eq!(
                rejection(&body),
                "expires_in must be at least 600 seconds",
                "{secs} is under the stock 600-second floor"
            );
        }
        let body = params(json!({"status": "hi", "expires_in": i64::MAX}));
        assert_eq!(
            rejection(&body),
            EXPIRATION_TOO_LARGE,
            "a span no timestamp can hold is a mistake, not a distant expiry"
        );
    }

    /// The refusal is generated from `MIN_EXPIRATION_SECS`: an instance
    /// that lowered the floor names its own number and takes its own
    /// minimum.
    #[test]
    fn the_message_names_this_instances_floor() {
        let config = Config {
            min_expiration_secs: 120,
            ..Config::for_tests()
        };
        let checked = |secs: i64| {
            let body = params(json!({"status": "hi", "expires_in": secs}));
            check_body(&config, "public", false, &body)
                .map(|_| ())
                .map_err(|err| err.to_string())
        };
        assert!(checked(120).is_ok(), "the configured floor itself passes");
        assert_eq!(
            checked(119).expect_err("one under it does not"),
            "expires_in must be at least 120 seconds",
            "and the message says the configured number, not the shipped one"
        );
        let stock = params(json!({"status": "hi", "expires_in": 120}));
        assert_eq!(
            check_body(&Config::for_tests(), "public", false, &stock)
                .expect_err("the stock floor still refuses it")
                .to_string(),
            "expires_in must be at least 600 seconds",
            "the default config keeps the default message"
        );
    }
}
