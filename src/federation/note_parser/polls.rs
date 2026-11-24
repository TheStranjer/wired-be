//! The `Question`'s poll: the `oneOf`/`anyOf` option list with its
//! per-option vote counts, and when voting closes.

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::federation::json::as_array;

use super::{
    MAX_POLL_OPTIONS, ParseError, TYPE_QUESTION,
    text::timestamp_field,
    types::{ParsedPoll, ParsedPollOption},
};

/// The poll of a `Question` — `oneOf` for a single-choice, `anyOf` for a
/// multi-choice — or `None` for every other type, whose option lists (if
/// any) are ignored. A `Question` with no usable option fails
/// [`ParseError::InvalidPoll`]; one with more than `MAX_POLL_OPTIONS` keeps
/// the first `MAX_POLL_OPTIONS`, in order.
pub(super) fn parse_poll(json: &Value, kind: &str) -> Result<Option<ParsedPoll>, ParseError> {
    if kind != TYPE_QUESTION {
        return Ok(None);
    }
    let (entries, multiple) = match (json.get("oneOf"), json.get("anyOf")) {
        (Some(options), _) => (options, false),
        (None, Some(options)) => (options, true),
        (None, None) => {
            return Err(ParseError::InvalidPoll(
                "the question carries neither oneOf nor anyOf".to_owned(),
            ));
        }
    };
    let mut options: Vec<ParsedPollOption> =
        as_array(entries).iter().filter_map(poll_option).collect();
    if options.is_empty() {
        return Err(ParseError::InvalidPoll(
            "the question carries no usable option".to_owned(),
        ));
    }
    options.truncate(MAX_POLL_OPTIONS);
    Ok(Some(ParsedPoll {
        options,
        multiple,
        expires_at: poll_expiry(json),
        voters_count: json.get("votersCount").and_then(Value::as_i64),
    }))
}

/// One poll option: its `name` and the `replies.totalItems` count as
/// reported; entries without a name are skipped.
fn poll_option(entry: &Value) -> Option<ParsedPollOption> {
    let title = entry.get("name").and_then(Value::as_str)?;
    if title.is_empty() {
        return None;
    }
    let votes_count = entry
        .get("replies")
        .and_then(|replies| replies.get("totalItems"))
        .and_then(Value::as_i64)
        .unwrap_or_default();
    Some(ParsedPollOption {
        title: title.to_owned(),
        votes_count,
    })
}

/// When voting closes: `endTime`, else a `closed` given as a timestamp
/// (Mastodon closes polls by rewriting `closed`; a boolean there carries
/// no time).
fn poll_expiry(json: &Value) -> Option<DateTime<Utc>> {
    ["endTime", "closed"]
        .iter()
        .find_map(|key| timestamp_field(json, key))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        federation::note_parser::{parse_note, test_support::*},
        util::time::parse_time,
    };

    #[test]
    fn parses_a_question_with_a_poll() {
        let parsed = parse(&fixture(QUESTION));
        assert_eq!(
            parsed.status.content, "<p>Which is better?</p>",
            "plain content"
        );
        let poll = parsed.poll.expect("a Question carries a poll");
        assert!(!poll.multiple, "oneOf is a single-choice poll");
        assert_eq!(
            poll.options,
            [
                ParsedPollOption {
                    title: "Red".to_owned(),
                    votes_count: 3
                },
                ParsedPollOption {
                    title: "Blue".to_owned(),
                    votes_count: 7
                },
            ],
            "names and reported counts, in order"
        );
        assert_eq!(
            poll.expires_at,
            Some(parse_time("2024-02-10T00:00:00.000Z").unwrap()),
            "endTime closes the vote"
        );
        assert_eq!(poll.voters_count, Some(10), "votersCount");
        assert_eq!(
            parsed.status.expires_at, poll.expires_at,
            "the status expires with its poll"
        );
    }

    #[test]
    fn any_of_is_a_multiple_choice_poll() {
        let mut document = fixture(QUESTION);
        document["anyOf"] = document["oneOf"].take();
        remove(&mut document, "oneOf");
        let poll = parse(&document).poll.expect("the poll survives the rename");
        assert!(poll.multiple, "anyOf lets voters tick twice");
        assert_eq!(poll.options.len(), 2, "and carries the same options");
    }

    /// A `Question` note carrying the given option list under `field`.
    fn question_with(field: &str, options: Value) -> Value {
        let mut document = minimal_note();
        document["type"] = json!("Question");
        document[field] = options;
        document
    }

    /// A JSON option list of `count` named options, `o0`, `o1`, ….
    fn named_options(count: usize) -> Value {
        json!(
            (0..count)
                .map(|n| json!({"name": format!("o{n}")}))
                .collect::<Vec<Value>>()
        )
    }

    #[test]
    fn questions_without_a_usable_option_fail() {
        for (name, document) in [
            ("no option list at all", minimal_note_with_question_type()),
            ("an empty list", question_with("anyOf", json!([]))),
            ("a null list", question_with("anyOf", Value::Null)),
            (
                "only nameless or empty-named entries",
                question_with("oneOf", json!([{"type": "Note"}, {"name": ""}])),
            ),
        ] {
            let error = parse_note(&document, &author(), now()).unwrap_err();
            assert!(
                matches!(error, ParseError::InvalidPoll(_)),
                "{name}: a bad question fails InvalidPoll, got {error}"
            );
        }
    }

    #[test]
    fn a_single_option_question_parses() {
        let poll = parse(&question_with("oneOf", json!([{"name": "only"}])))
            .poll
            .expect("one option is accepted");
        assert_eq!(poll.options.len(), 1, "the lone option is kept");
        assert_eq!(poll.options[0].title, "only", "with its title");
    }

    #[test]
    fn a_question_past_the_cap_is_truncated_in_order() {
        let over = MAX_POLL_OPTIONS + 10;
        let poll = parse(&question_with("oneOf", named_options(over)))
            .poll
            .expect("an oversized poll still parses");
        let titles: Vec<String> = poll.options.iter().map(|o| o.title.clone()).collect();
        let expected: Vec<String> = (0..MAX_POLL_OPTIONS).map(|n| format!("o{n}")).collect();
        assert_eq!(titles, expected, "the first options are kept, in order");
    }

    #[test]
    fn twenty_five_options_all_survive_under_the_cap() {
        let poll = parse(&question_with("anyOf", named_options(25)))
            .poll
            .expect("25 options parse");
        let expected = 25.min(MAX_POLL_OPTIONS);
        assert_eq!(poll.options.len(), expected, "up to the cap are kept");
        assert_eq!(poll.options[0].title, "o0", "starting from the first");
        assert_eq!(
            poll.options[expected - 1].title,
            format!("o{}", expected - 1),
            "in document order"
        );
    }

    /// A public note with the `Question` type and no option lists.
    fn minimal_note_with_question_type() -> Value {
        let mut document = minimal_note();
        document["type"] = json!("Question");
        document
    }

    #[test]
    fn a_non_question_ignores_its_option_lists() {
        let mut document = minimal_note();
        document["oneOf"] = json!([{"name": "stray"}]);
        let parsed = parse(&document);
        assert!(parsed.poll.is_none(), "a Note is never a poll");
    }

    #[test]
    fn a_closed_question_expires_at_its_closed_stamp() {
        let mut document = question_with("oneOf", json!([{"name": "a"}, {"name": "b"}]));
        document["closed"] = json!("2024-02-09T00:00:00Z");
        let poll = parse(&document).poll.expect("the poll parses");
        assert_eq!(
            poll.expires_at,
            Some(parse_time("2024-02-09T00:00:00Z").unwrap()),
            "a timestamp closed names the end"
        );
        assert_eq!(poll.voters_count, None, "no votersCount, no count");
        assert_eq!(
            poll.options[0].votes_count, 0,
            "no replies block is zero votes"
        );
    }
}
