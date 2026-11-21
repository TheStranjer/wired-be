//! The `Question`'s poll: the `oneOf`/`anyOf` option list with its
//! per-option vote counts, and when voting closes.

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::federation::json::as_array;

use super::{
    MAX_POLL_OPTIONS, MIN_POLL_OPTIONS, ParseError, TYPE_QUESTION,
    text::timestamp_field,
    types::{ParsedPoll, ParsedPollOption},
};

/// The poll of a `Question` — `oneOf` for a single-choice, `anyOf` for a
/// multi-choice — or `None` for every other type, whose option lists (if
/// any) are ignored. An invalid `Question` list fails [`ParseError::InvalidPoll`].
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
    let options: Vec<ParsedPollOption> = as_array(entries).iter().filter_map(poll_option).collect();
    if options.len() < MIN_POLL_OPTIONS || options.len() > MAX_POLL_OPTIONS {
        return Err(ParseError::InvalidPoll(format!(
            "the question carries {} options, {MIN_POLL_OPTIONS} to {MAX_POLL_OPTIONS} are required",
            options.len()
        )));
    }
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

    #[test]
    fn questions_need_two_to_twenty_options() {
        let one = json!([{"name": "only"}]);
        let twenty_one = json!(
            (0..21)
                .map(|n| json!({"name": format!("o{n}")}))
                .collect::<Vec<Value>>()
        );
        for (name, document) in [
            ("no option list at all", minimal_note_with_question_type()),
            ("one option", question_with("oneOf", one)),
            ("twenty-one options", question_with("oneOf", twenty_one)),
            ("an empty list", question_with("anyOf", json!([]))),
            ("a null list", question_with("anyOf", Value::Null)),
        ] {
            let error = parse_note(&document, &author(), now()).unwrap_err();
            assert!(
                matches!(error, ParseError::InvalidPoll(_)),
                "{name}: a bad question fails InvalidPoll, got {error}"
            );
        }
        let two = json!([{"name": "a"}, {"name": "b"}]);
        let ok = parse(&question_with("anyOf", two));
        assert_eq!(
            ok.poll
                .expect("two options is the minimum, accepted")
                .options
                .len(),
            2,
            "exactly two options parses"
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
