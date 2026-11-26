//! What a remote video or audio `attachment` entry says beyond its url:
//! its preview image and its length.
//!
//! Peers state the preview as an `icon` (`PeerTube`, Pleroma) or `preview`
//! (`ActivityStreams`) `Image`, as a bare url, an object, or an array of
//! either; the length as an `ISO 8601` `duration` (`PT1M30S`) or a plain
//! number of seconds. Neither is trusted: a preview must be an `http(s)`
//! url and a length a positive, finite, plausible number of seconds, and
//! anything else is dropped without costing the attachment.

use serde_json::Value;

use crate::{
    federation::json::{as_array, first_string},
    util::url::is_http_url,
};

/// The keys a preview image may sit under, in preference order.
const PREVIEW_KEYS: [&str; 2] = ["icon", "preview"];

/// The longest duration kept, in seconds: a year, past which a stated
/// length is a bug or an attack, not a recording.
const MAX_DURATION_SECONDS: f64 = 31_536_000.0;

/// The `http(s)` url of the entry's preview image: the first usable one
/// under `icon`, then `preview`.
pub(super) fn preview_url(entry: &Value) -> Option<String> {
    PREVIEW_KEYS
        .iter()
        .filter_map(|key| entry.get(*key))
        .flat_map(as_array)
        .filter_map(first_string)
        .find(|candidate| is_http_url(candidate))
}

/// The entry's length in seconds, from a numeric or an `ISO 8601`
/// `duration`; `None` when it is absent, unreadable, zero, negative or
/// over [`MAX_DURATION_SECONDS`].
pub(super) fn duration_seconds(entry: &Value) -> Option<f64> {
    let seconds = match entry.get("duration")? {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => iso_seconds(text).or_else(|| text.trim().parse().ok())?,
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => return None,
    };
    (seconds.is_finite() && seconds > 0.0 && seconds <= MAX_DURATION_SECONDS).then_some(seconds)
}

/// The seconds an `ISO 8601` duration (`P1DT2H3M4.5S`) states, `None` for
/// anything else — weeks, months and years are not fixed lengths and are
/// refused.
fn iso_seconds(text: &str) -> Option<f64> {
    let body = text.trim().strip_prefix('P')?;
    let (date, time) = body.split_once('T').unwrap_or((body, ""));
    let days = sum_components(date, &[('D', 86_400.0)])?;
    let clock = sum_components(time, &[('H', 3_600.0), ('M', 60.0), ('S', 1.0)])?;
    Some(days + clock)
}

/// The seconds of a run of `<number><unit>` components, each unit worth
/// the scale `units` gives it; `None` for an unknown unit or a bad number.
fn sum_components(text: &str, units: &[(char, f64)]) -> Option<f64> {
    let mut rest = text;
    let mut total = 0.0;
    while !rest.is_empty() {
        let unit_at = rest.find(|candidate: char| candidate.is_ascii_alphabetic())?;
        let (number, from_unit) = rest.split_at_checked(unit_at)?;
        let unit = from_unit.chars().next()?;
        let scale = units
            .iter()
            .find_map(|(known, scale)| (*known == unit).then_some(*scale))?;
        let amount: f64 = number.parse().ok()?;
        if !amount.is_finite() || amount < 0.0 {
            return None;
        }
        total = amount.mul_add(scale, total);
        rest = from_unit.get(unit.len_utf8()..)?;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The duration an entry stating `value` reads as.
    fn read(value: &Value) -> Option<f64> {
        duration_seconds(&json!({ "duration": value }))
    }

    #[test]
    fn a_preview_is_read_from_icon_or_preview_in_every_shape() {
        for (label, entry) in [
            (
                "an icon object",
                json!({"icon": {"type": "Image", "url": "https://x.example/t.jpg"}}),
            ),
            (
                "an icon array",
                json!({"icon": [{"type": "Image", "url": "https://x.example/t.jpg"}]}),
            ),
            (
                "a bare icon url",
                json!({"icon": "https://x.example/t.jpg"}),
            ),
            (
                "a preview object",
                json!({"preview": {"type": "Image", "url": "https://x.example/t.jpg"}}),
            ),
            (
                "an icon url link",
                json!({"icon": {"url": {"type": "Link", "href": "https://x.example/t.jpg"}}}),
            ),
            (
                "a bad icon then a good preview",
                json!({"icon": "mailto:x@example.org", "preview": "https://x.example/t.jpg"}),
            ),
        ] {
            assert_eq!(
                preview_url(&entry).as_deref(),
                Some("https://x.example/t.jpg"),
                "{label}"
            );
        }
    }

    #[test]
    fn an_unusable_preview_is_nothing() {
        for entry in [
            json!({}),
            json!({"icon": null}),
            json!({"icon": 7}),
            json!({"icon": "data:image/png;base64,AAAA"}),
            json!({"icon": {"type": "Image"}}),
            json!({"icon": []}),
        ] {
            assert_eq!(preview_url(&entry), None, "{entry}");
        }
    }

    #[test]
    fn iso_and_numeric_durations_read_as_seconds() {
        for (value, expected) in [
            (json!("PT3.75S"), 3.75),
            (json!("PT1M30S"), 90.0),
            (json!("PT1H2M3S"), 3_723.0),
            (json!("P1DT1S"), 86_401.0),
            (json!(" PT5S "), 5.0),
            (json!(12.5), 12.5),
            (json!(7), 7.0),
            (json!("42.5"), 42.5),
        ] {
            assert_eq!(read(&value), Some(expected), "{value}");
        }
    }

    #[test]
    fn a_bad_duration_is_nothing() {
        for value in [
            json!("P1W"),
            json!("P1Y"),
            json!("PT"),
            json!("P"),
            json!("PT-5S"),
            json!("PT1X"),
            json!("PT1.2.3S"),
            json!("PT5"),
            json!("soon"),
            json!(0),
            json!(-3),
            json!(31_536_001),
            json!(true),
            json!([1]),
            json!({"seconds": 1}),
            Value::Null,
        ] {
            assert_eq!(read(&value), None, "{value}");
        }
        assert_eq!(duration_seconds(&json!({})), None, "no duration at all");
    }
}
