//! Timestamps: the Mastodon wire format, its serde glue, and parsing input.
//!
//! Per `docs/ARCHITECTURE.md` decision 5 timestamps are stored as
//! `timestamptz`, kept in Rust as `chrono::DateTime<Utc>` and written to JSON in
//! the Mastodon style `2024-01-31T12:34:56.789Z`: UTC, exactly three fractional
//! digits, a literal `Z`.

use chrono::{DateTime, Utc};

/// `chrono` format for [`mastodon_time`]: date and time at millisecond
/// precision with a literal `Z` suffix.
const MASTODON_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3fZ";
/// What the serde helpers report for text that is not a timestamp.
const INVALID_TIME: &str = "expected an RFC 3339 timestamp such as 2024-01-31T12:34:56.789Z";

/// Formats `dt` the way Mastodon clients expect, e.g.
/// `2024-01-31T12:34:56.789Z`.
///
/// The output always carries exactly three fractional digits, so a whole second
/// becomes `.000Z`, and always ends in `Z`. Anything finer than a millisecond is
/// dropped.
#[must_use]
pub fn mastodon_time(dt: &DateTime<Utc>) -> String {
    dt.format(MASTODON_FORMAT).to_string()
}

/// Reads an RFC 3339 timestamp into UTC, `None` for anything else.
///
/// Both the `Z` and the `+hh:mm` spellings of the offset are accepted, the
/// fractional part may have any number of digits, and the result is normalized
/// to UTC. The parser is chrono's, which is forgiving about the `T` between
/// date and time (a space works too); nothing else is.
#[must_use]
pub fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Serde support for a `DateTime<Utc>` field in the Mastodon format.
///
/// Use it as `#[serde(with = "crate::util::time::serde_mastodon_time")]`;
/// [`serde_mastodon_time::option`] is the matching helper for an
/// `Option<DateTime<Utc>>` field.
pub mod serde_mastodon_time {
    use chrono::{DateTime, Utc};
    use serde::{Deserialize as _, Deserializer, Serializer, de::Error as _};

    use super::{INVALID_TIME, mastodon_time, parse_time};

    /// Writes `dt` out as a Mastodon timestamp string.
    ///
    /// # Errors
    ///
    /// Whatever [`Serializer`] reports for a string value.
    pub fn serialize<S>(dt: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&mastodon_time(dt))
    }

    /// Reads a Mastodon timestamp string into UTC.
    ///
    /// # Errors
    ///
    /// A serde error when the value is not a string, or not an RFC 3339
    /// timestamp.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        parse_time(&text).ok_or_else(|| D::Error::custom(INVALID_TIME))
    }

    /// Serde support for an `Option<DateTime<Utc>>` field in the Mastodon
    /// format.
    ///
    /// Use it as
    /// `#[serde(with = "crate::util::time::serde_mastodon_time::option")]`.
    /// `None` becomes JSON `null`, and JSON `null` becomes `None`.
    pub mod option {
        use chrono::{DateTime, Utc};
        use serde::{Deserialize as _, Deserializer, Serializer, de::Error as _};

        use super::{INVALID_TIME, mastodon_time, parse_time};

        /// The shape serde hands to a `with` module's `serialize`: a borrow of
        /// the whole `Option<DateTime<Utc>>` field. serde fixes that signature,
        /// so it is spelled out here instead of inlined in the parameter list.
        type Field<'a> = &'a Option<DateTime<Utc>>;

        /// Writes a present timestamp as a Mastodon string, `None` as `null`.
        ///
        /// # Errors
        ///
        /// Whatever [`Serializer`] reports for a string value.
        pub fn serialize<S>(dt: Field<'_>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            match dt {
                Some(dt) => serializer.serialize_some(&mastodon_time(dt)),
                None => serializer.serialize_none(),
            }
        }

        /// Reads an optional Mastodon timestamp string into UTC.
        ///
        /// # Errors
        ///
        /// A serde error when the value is a string that is not an RFC 3339
        /// timestamp.
        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<DateTime<Utc>>, D::Error>
        where
            D: Deserializer<'de>,
        {
            Option::<String>::deserialize(deserializer)?
                .map(|text| parse_time(&text).ok_or_else(|| D::Error::custom(INVALID_TIME)))
                .transpose()
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, TimeZone as _};
    use serde::{Deserialize, Serialize};

    use super::*;

    /// A timestamp with milliseconds, as it goes on the wire.
    const WIRE: &str = "2024-01-31T12:34:56.789Z";
    /// The same instant with a `+01:00` offset instead of `Z`.
    const WIRE_PLUS_ONE: &str = "2024-01-31T13:34:56.789+01:00";

    /// 2024-01-31T12:34:56.789Z, built from its parts so that a test never
    /// trusts [`parse_time`] to check itself.
    fn utc_789ms() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 1, 31, 12, 34, 56)
            .unwrap()
            .checked_add_signed(TimeDelta::milliseconds(789))
            .unwrap()
    }

    /// A JSON object with both serde helpers in play: [`Sample::at`] uses the
    /// required variant, [`Sample::maybe`] the optional one.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Sample {
        /// A timestamp that must be there.
        #[serde(with = "crate::util::time::serde_mastodon_time")]
        at: DateTime<Utc>,
        /// A timestamp that may be absent.
        #[serde(with = "crate::util::time::serde_mastodon_time::option")]
        maybe: Option<DateTime<Utc>>,
    }

    /// A sample whose required timestamp is `value`, its optional one unset.
    fn required_only(value: &str) -> String {
        format!(r#"{{"at":"{value}","maybe":null}}"#)
    }

    #[test]
    fn formats_a_timestamp_with_three_fractional_digits() {
        assert_eq!(mastodon_time(&utc_789ms()), WIRE, "the reference format");
        assert_eq!(
            mastodon_time(&parse_time(WIRE).unwrap()),
            WIRE,
            "formatting is stable over a parsed timestamp"
        );
    }

    #[test]
    fn pads_milliseconds_to_three_digits() {
        let whole_second = parse_time("2024-01-31T12:34:56Z").unwrap();
        assert_eq!(
            mastodon_time(&whole_second),
            "2024-01-31T12:34:56.000Z",
            "0 ms is written as .000Z"
        );
        let one_digit = parse_time("2024-01-31T12:34:56.7Z").unwrap();
        assert_eq!(
            mastodon_time(&one_digit),
            "2024-01-31T12:34:56.700Z",
            "tenths are padded out to milliseconds"
        );
        let one_ms = parse_time("2024-01-31T12:34:56.001Z").unwrap();
        assert_eq!(
            mastodon_time(&one_ms),
            "2024-01-31T12:34:56.001Z",
            "one millisecond is padded with zeros"
        );
    }

    #[test]
    fn drops_precision_below_a_millisecond() {
        let dt = parse_time("2024-01-31T12:34:56.789999Z").unwrap();
        assert_eq!(
            mastodon_time(&dt),
            "2024-01-31T12:34:56.789Z",
            "microseconds are cut off, not rounded up"
        );
    }

    #[test]
    fn parses_rfc3339_into_utc() {
        assert_eq!(parse_time(WIRE), Some(utc_789ms()), "the wire format");
        assert_eq!(
            parse_time(WIRE_PLUS_ONE),
            Some(utc_789ms()),
            "an offset is converted to UTC"
        );
        assert_eq!(
            mastodon_time(&parse_time(WIRE_PLUS_ONE).unwrap()),
            WIRE,
            "and written back as Z"
        );
    }

    #[test]
    fn rejects_text_that_is_not_a_timestamp() {
        for text in [
            "",
            "not-a-time",
            "2024-01-31",
            "2024-01-31T12:34:56",
            "2024-01-31T12:34:56.789",
            "20240131T123456Z",
            "1706722496",
        ] {
            assert_eq!(parse_time(text), None, "{text} is not a timestamp");
        }
    }

    #[test]
    fn accepts_a_space_where_the_spec_wants_a_t() {
        assert_eq!(
            parse_time("2024-01-31 12:34:56.789Z"),
            Some(utc_789ms()),
            "chrono's parser allows the space separator"
        );
    }

    #[test]
    fn serializes_both_variants() {
        let sample = Sample {
            at: utc_789ms(),
            maybe: Some(utc_789ms()),
        };
        assert_eq!(
            serde_json::to_string(&sample).unwrap(),
            format!(r#"{{"at":"{WIRE}","maybe":"{WIRE}"}}"#),
            "both fields use the Mastodon format"
        );
    }

    #[test]
    fn round_trips_a_present_optional_time() {
        let sample = Sample {
            at: utc_789ms(),
            maybe: Some(utc_789ms()),
        };
        let json = serde_json::to_string(&sample).unwrap();
        assert_eq!(
            serde_json::from_str::<Sample>(&json).unwrap(),
            sample,
            "a present timestamp survives the trip"
        );
    }

    #[test]
    fn none_serializes_as_null_and_round_trips() {
        let sample = Sample {
            at: utc_789ms(),
            maybe: None,
        };
        let json = serde_json::to_string(&sample).unwrap();
        assert_eq!(json, required_only(WIRE), "None is JSON null, not a string");
        assert_eq!(
            serde_json::from_str::<Sample>(&json).unwrap(),
            sample,
            "null comes back as None"
        );
    }

    #[test]
    fn rejects_a_null_required_time() {
        let json = format!(r#"{{"at":null,"maybe":"{WIRE}"}}"#);
        let err =
            serde_json::from_str::<Sample>(&json).expect_err("a required time cannot be null");
        assert!(
            err.to_string().contains("invalid type"),
            "the error says what was wrong: {err}"
        );
    }

    #[test]
    fn rejects_an_unparsable_time_in_either_variant() {
        for json in [
            required_only("nope"),
            format!(r#"{{"at":"{WIRE}","maybe":"nope"}}"#),
        ] {
            let err =
                serde_json::from_str::<Sample>(&json).expect_err("neither body is a timestamp");
            assert!(
                err.to_string().contains("RFC 3339"),
                "both helpers reject bad text: {err}"
            );
        }
    }
}
