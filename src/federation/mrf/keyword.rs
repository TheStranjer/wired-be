//! Pleroma's `KeywordPolicy`: keyword and regex filters of the
//! [pipeline](super).
//!
//! The reference is `lib/pleroma/web/activity_pub/mrf/keyword_policy.ex` —
//! "reject or word-replace activities with a keyword or regex". Three lists
//! under the `keyword` key of the `mrf` instance setting act on the text of
//! the note a `Create` or `Update` carries:
//!
//! ```json
//! {"policies": ["keyword"],
//!  "keyword": {"reject": ["free money", "/^buy\\s+now\\b/i"],
//!              "federated_timeline_removal": ["politics"],
//!              "replace": [["bad word", "b*tch"], ["/\\bfoo\\b/i", "bar"]]}}
//! ```
//!
//! Every pattern entry is either a plain string — matched as a
//! case-insensitive substring — or a regex written between slashes,
//! `"/pattern/"`, optionally followed by the inline flags `i`, `m`, `s`
//! or `x` (`"/pattern/i"`), compiled with the `regex` crate under a
//! compiled-program size limit. A pattern that does not compile (or
//! outgrows the limit) is warned about and skipped; the rest of its list
//! keeps working. `replace` entries are `[pattern, replacement]` pairs,
//! and a replacement is written literally: no `$name` group expansion.
//! A `/`-prefixed entry whose last slash leaves non-flag text behind
//! (`"/only"`, `"/a/b"`) is just a string with slashes in it.
//!
//! The three lists are weighed in Pleroma's order — `reject`, then
//! `federated_timeline_removal`, then `replace` — against the note's
//! `content`, `summary` and `name` joined by newlines, so a replacement
//! never hides text the `reject` list should have seen:
//!
//! * [`List::Reject`] refuses the activity outright.
//! * [`List::FederatedTimelineRemoval`] moves the public address from `to`
//!   into `cc` — the note arrives unlisted, exactly as the
//!   [`simple`](super::simple) policy's list of the same name does, reached
//!   by keyword instead of by domain.
//! * [`List::Replace`] rewrites `content`, `summary` and `name` of the
//!   embedded note: every entry applies to every one of those fields that
//!   holds a string, in configured order, replacing all matches.
//!
//! The lists are compiled once, when the configuration is read — beside
//! the block [`describe`](MrfPolicy::describe) echoes — so a busy inbox
//! matches instead of reparsing and recompiling patterns per activity.

use std::sync::Arc;

use regex::{NoExpand, Regex, RegexBuilder, escape};
use serde_json::{Map, Value, json};
use tracing::warn;

use crate::federation::mrf::{
    MrfContext, MrfPolicy, MrfResult, Transparency,
    simple::{is_note_activity, note_object_mut, rewrite_each_document, unlist_document},
    single_block,
};

/// The key of this policy's parameters inside the `mrf` instance setting,
/// and the name [`build_chain`](super::build_chain) resolves.
pub const PARAMS_KEY: &str = "keyword";
/// The compiled-program size limit every pattern is built under: a remote
/// operator may not pin this instance's inbox with a pathological regex.
const REGEX_SIZE_LIMIT: usize = 1 << 20;
/// The delimiter pair that marks an entry as a regex: `"/pattern/"`.
const REGEX_DELIMITER: char = '/';
/// The flag characters a regex entry may carry after its closing slash,
/// applied as the `regex` crate's inline `(?flags)` group.
const REGEX_FLAGS: &str = "imsx";
/// The note fields whose text this policy reads and rewrites, in the order
/// they join into the payload the pattern lists match against.
const TEXT_FIELDS: [&str; 3] = ["content", "summary", "name"];
/// Pleroma's rejection message for a note the `reject` list names.
const REJECT_REASON: &str = "[KeywordPolicy] Matches with rejected keyword";

/// One of the policy's three configured lists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum List {
    /// `reject`: refuse activities whose payload a pattern matches.
    Reject,
    /// `federated_timeline_removal`: unlist activities whose payload a
    /// pattern matches.
    FederatedTimelineRemoval,
    /// `replace`: rewrite the note's text with `[pattern, replacement]`
    /// pairs.
    Replace,
}

impl List {
    /// Every list, in the order Pleroma applies them.
    pub const ALL: [Self; 3] = [Self::Reject, Self::FederatedTimelineRemoval, Self::Replace];

    /// The key the list is configured under.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Reject => "reject",
            Self::FederatedTimelineRemoval => "federated_timeline_removal",
            Self::Replace => "replace",
        }
    }

    /// The list a configuration key names, `None` for any other key.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|list| list.key() == key)
    }
}

/// One compiled pattern entry: a plain string as its case-insensitive
/// substring matcher, or the regex the operator wrote.
#[derive(Clone, Debug)]
struct Pattern {
    /// The compiled matcher.
    matcher: Regex,
}

impl Pattern {
    /// Compiles one configured pattern source into its matcher, `None`
    /// (beside the warning that says why) for a pattern that does not
    /// compile or outgrows [`REGEX_SIZE_LIMIT`]. A `"/…/"` source compiles
    /// as the regex between the delimiters, its trailing flags applied as
    /// an inline `(?flags)` group; anything else as a case-insensitive
    /// literal substring of itself.
    fn compile(source: &str) -> Option<Self> {
        let (pattern, literal) = match regex_parts(source) {
            Some((body, "")) => (body.to_owned(), false),
            Some((body, flags)) => (format!("(?{flags}){body}"), false),
            None => (escape(source), true),
        };
        let compiled = RegexBuilder::new(&pattern)
            .case_insensitive(literal)
            .size_limit(REGEX_SIZE_LIMIT)
            .build();
        compiled
            .inspect_err(|err| {
                warn!(
                    entry = %source,
                    %err,
                    "an `mrf.keyword` pattern does not compile or is over the size limit; the entry is skipped"
                );
            })
            .ok()
            .map(|matcher| Self { matcher })
    }

    /// Whether `text` carries this pattern.
    fn matches(&self, text: &str) -> bool {
        self.matcher.is_match(text)
    }
}

/// The body and trailing flags of a `"/pattern/flags"` regex entry, `None`
/// for a plain string: an entry must open with a delimiter and carry a
/// closing one whose remainder is nothing but [`REGEX_FLAGS`] characters.
/// The lone delimiter (`"/"`) is a one-character string, not a regex.
fn regex_parts(source: &str) -> Option<(&str, &str)> {
    let (body, flags) = source
        .strip_prefix(REGEX_DELIMITER)?
        .rsplit_once(REGEX_DELIMITER)?;
    flags
        .chars()
        .all(|flag| REGEX_FLAGS.contains(flag))
        .then_some((body, flags))
}

/// One compiled `replace` entry: the pattern beside its literal
/// replacement text.
#[derive(Clone, Debug)]
struct Replacement {
    /// The pattern to rewrite.
    pattern: Pattern,
    /// The text that takes its place, written verbatim.
    replacement: String,
}

impl Replacement {
    /// Reads one configured `[pattern, replacement]` pair, `None` (beside
    /// the warnings that say why) for any other shape or an uncompilable
    /// pattern.
    fn from_value(value: &Value) -> Option<Self> {
        let Value::Array(parts) = value else {
            warn!(entry = %value, "an `mrf.keyword` replace entry is not a [pattern, replacement] pair; ignored");
            return None;
        };
        let [Value::String(pattern), Value::String(replacement)] = parts.as_slice() else {
            warn!(entry = %value, "an `mrf.keyword` replace entry is not a [pattern, replacement] pair of strings; ignored");
            return None;
        };
        let pattern = Pattern::compile(pattern)?;
        Some(Self {
            pattern,
            replacement: replacement.clone(),
        })
    }
}

/// The compiled `keyword` configuration: the policy's three lists, built
/// once per configuration load and shared by every chain that runs them.
#[derive(Clone, Debug)]
pub struct KeywordConfig {
    /// The parameter block as configured, for
    /// [`describe`](crate::federation::mrf::MrfPolicy::describe); `None`
    /// when the setting carries none or a non-object.
    params: Option<Value>,
    /// The compiled `reject` list.
    reject: Vec<Pattern>,
    /// The compiled `federated_timeline_removal` list.
    delist: Vec<Pattern>,
    /// The compiled `replace` list.
    replace: Vec<Replacement>,
}

impl KeywordConfig {
    /// Compiles the `keyword` key of the `mrf` instance setting. `params`
    /// is that key's value, `None` when the setting carries none; a
    /// missing key or a non-object document reads as the empty
    /// configuration — the policy then governs nothing, and the warning
    /// says what was dropped. Malformed entries drop out of their lists
    /// one by one, each with its own warning.
    #[must_use]
    pub fn from_params(params: Option<&Value>) -> Self {
        let fields = params.and_then(Value::as_object);
        if params.is_some() && fields.is_none() {
            warn!("the `keyword` MRF setting is not a JSON object; the policy governs nothing");
        }
        Self {
            params: fields.cloned().map(Value::Object),
            reject: pattern_list(fields, List::Reject),
            delist: pattern_list(fields, List::FederatedTimelineRemoval),
            replace: replacement_list(fields),
        }
    }

    /// Folds the `replace` entries over the note's string text fields, in
    /// configured order: every entry rewrites the whole field before the
    /// next one reads it, and replaces every match it makes.
    fn replace_in(&self, activity: &mut Value) {
        if self.replace.is_empty() {
            return;
        }
        let Some(object) = note_object_mut(activity) else {
            return;
        };
        for field in TEXT_FIELDS {
            let Some(Value::String(text)) = object.get_mut(field) else {
                continue;
            };
            for entry in &self.replace {
                let rewritten = entry
                    .pattern
                    .matcher
                    .replace_all(text, NoExpand(&entry.replacement))
                    .into_owned();
                *text = rewritten;
            }
        }
    }
}

/// The compiled pattern list one of the policy's keys holds: every usable
/// entry in configured order; anything malformed warns and drops out.
fn pattern_list(fields: Option<&Map<String, Value>>, list: List) -> Vec<Pattern> {
    let Some(entries) = list_entries(fields, list) else {
        return Vec::new();
    };
    entries.iter().filter_map(pattern_entry).collect()
}

/// The compiled pattern behind one configured list entry; anything but a
/// string pattern warns and drops out.
fn pattern_entry(entry: &Value) -> Option<Pattern> {
    let Some(text) = entry.as_str() else {
        warn!(
            entry = %entry,
            "an `mrf.keyword` pattern entry is not a string; ignored"
        );
        return None;
    };
    Pattern::compile(text)
}

/// The compiled `replace` pairs of the configured key.
fn replacement_list(fields: Option<&Map<String, Value>>) -> Vec<Replacement> {
    let Some(entries) = list_entries(fields, List::Replace) else {
        return Vec::new();
    };
    entries.iter().filter_map(Replacement::from_value).collect()
}

/// The configured array behind one of the policy's list keys, `None` (after
/// the warning that says why, when the key is there but holds something
/// else) when the document carries no usable list for it.
fn list_entries(fields: Option<&Map<String, Value>>, list: List) -> Option<&Vec<Value>> {
    let value = fields?.get(list.key())?;
    let Value::Array(entries) = value else {
        warn!(
            key = list.key(),
            "an `mrf.keyword` list is not an array; the list stays empty"
        );
        return None;
    };
    Some(entries)
}

/// The `content` + `summary` + `name` of the embedded note, joined by
/// newlines — the payload Pleroma matches the pattern lists against. A
/// field the note does not carry (or does not carry as a string) joins
/// nothing.
fn note_payload(activity: &Value) -> String {
    let Some(object) = activity.get("object").and_then(Value::as_object) else {
        return String::new();
    };
    TEXT_FIELDS
        .iter()
        .filter_map(|field| object.get(*field).and_then(Value::as_str))
        .collect::<Vec<&str>>()
        .join("\n")
}

/// The keyword stage: Pleroma's `MRF.KeywordPolicy`, the chain stage that
/// refuses, unlists or rewrites notes by their text.
#[derive(Clone, Debug)]
pub struct KeywordPolicy {
    /// The compiled lists, shared with the configuration that built them.
    config: Arc<KeywordConfig>,
}

impl KeywordPolicy {
    /// The policy over the compiled `keyword` configuration `config`.
    #[must_use]
    pub const fn new(config: Arc<KeywordConfig>) -> Self {
        Self { config }
    }

    /// Whether any pattern of `patterns` matches `payload`.
    fn any_matches(patterns: &[Pattern], payload: &str) -> bool {
        patterns.iter().any(|pattern| pattern.matches(payload))
    }
}

impl MrfPolicy for KeywordPolicy {
    fn name(&self) -> &'static str {
        "KeywordPolicy"
    }

    fn filter(&self, _ctx: &MrfContext, activity: Value) -> MrfResult {
        if !is_note_activity(&activity) {
            return Ok(activity);
        }
        let payload = note_payload(&activity);
        if Self::any_matches(&self.config.reject, &payload) {
            return Err(REJECT_REASON.to_owned());
        }
        let mut activity = activity;
        if Self::any_matches(&self.config.delist, &payload) {
            rewrite_each_document(&mut activity, unlist_document);
        }
        self.config.replace_in(&mut activity);
        Ok(activity)
    }

    fn describe(&self, _transparency: &Transparency) -> Map<String, Value> {
        single_block(
            self.name(),
            self.config.params.clone().unwrap_or_else(|| json!({})),
        )
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::federation::mrf::{MrfConfig, simple::PUBLIC_ADDRESS};

    /// The author the fixtures post as.
    const ALICE: &str = "https://peer.example/users/alice";
    /// The public address, full `ActivityStreams` spelling.
    const PUBLIC: &str = PUBLIC_ADDRESS;
    /// The author's followers collection, the `/followers` convention.
    const FOLLOWERS: &str = "https://peer.example/users/alice/followers";
    /// The note a fixture `Create` announces.
    const NOTE: &str = "https://peer.example/objects/1";

    /// The context of a run: nothing this policy reads from it.
    fn ctx() -> MrfContext {
        MrfContext {
            config: Arc::new(MrfConfig::assemble(
                vec![PARAMS_KEY.to_owned()],
                Map::from_iter([(PARAMS_KEY.to_owned(), json!({}))]),
            )),
            now: Utc::now(),
            local_domain: "test.example".to_owned(),
            author_follower_count: None,
            author_status_count: None,
            author_tags: Vec::new(),
            target_tags: Vec::new(),
        }
    }

    /// The policy over the `keyword` block `config` configures.
    fn policy(config: Option<&Value>) -> KeywordPolicy {
        KeywordPolicy::new(Arc::new(KeywordConfig::from_params(config)))
    }

    /// One policy run on `activity` under the block `config`.
    fn filtered(config: &Value, activity: Value) -> MrfResult {
        policy(Some(config)).filter(&ctx(), activity)
    }

    /// A public `Create` of a note carrying `content`, `summary` and
    /// `name`, by alice.
    fn note(content: &str, summary: &str, name: &str) -> Value {
        json!({
            "id": "https://peer.example/activities/1",
            "type": "Create",
            "actor": ALICE,
            "to": [PUBLIC],
            "cc": [FOLLOWERS],
            "object": {
                "id": NOTE,
                "type": "Note",
                "attributedTo": ALICE,
                "to": [PUBLIC],
                "cc": [FOLLOWERS],
                "content": content,
                "summary": summary,
                "name": name,
            },
        })
    }

    /// The `federated_timeline_removal` golden of `activity`: the public
    /// address out of `to` and onto the end of `cc`, on the activity and
    /// its note alike.
    fn delisted(activity: Value) -> Value {
        let mut expected = activity;
        expected["to"] = json!([]);
        expected["cc"] = json!([FOLLOWERS, PUBLIC]);
        expected["object"]["to"] = json!([]);
        expected["object"]["cc"] = json!([FOLLOWERS, PUBLIC]);
        expected
    }

    /// The same fixture note as an `Update`.
    fn updated(content: &str) -> Value {
        let mut activity = note(content, "", "");
        activity["type"] = json!("Update");
        activity
    }

    /// The table: one configured block, one activity, one golden answer.
    struct Case {
        /// What the case is about, for the assertion message.
        label: &'static str,
        /// The `keyword` block under test.
        config: Value,
        /// The activity the chain hands the policy.
        activity: Value,
        /// The golden answer: the accepted document or the rejection.
        expected: MrfResult,
    }

    /// Runs the table, one golden JSON answer per case.
    fn check_cases(cases: &[Case]) {
        for case in cases {
            assert_eq!(
                filtered(&case.config, case.activity.clone()),
                case.expected.clone(),
                "case: {}",
                case.label
            );
        }
    }

    /// `reject`: a plain entry matches its substring whatever the case, on
    /// any of the three text fields, and the golden rejection names the
    /// policy Pleroma's way.
    #[test]
    fn a_reject_substring_refuses_the_note_case_insensitively() {
        check_cases(&[
            Case {
                label: "content hit",
                config: json!({"reject": ["free money"]}),
                activity: note("<p>FREE MONEY!!!</p>", "", ""),
                expected: Err(REJECT_REASON.to_owned()),
            },
            Case {
                label: "summary hit",
                config: json!({"reject": ["scam"]}),
                activity: note("<p>hello</p>", "a SCAM warning", ""),
                expected: Err(REJECT_REASON.to_owned()),
            },
            Case {
                label: "name hit",
                config: json!({"reject": ["sale"]}),
                activity: note("<p>hello</p>", "", "Weekly SALE"),
                expected: Err(REJECT_REASON.to_owned()),
            },
            Case {
                label: "no hit",
                config: json!({"reject": ["free money"]}),
                activity: note("<p>hello</p>", "", ""),
                expected: Ok(note("<p>hello</p>", "", "")),
            },
        ]);
    }

    /// `reject`: a `"/…/"` entry compiles as a regex — an anchored pattern
    /// refuses what the same text read as a plain substring would treat
    /// differently, inline flags apply, and a `/` that closes nothing is
    /// just a character of the literal.
    #[test]
    fn a_reject_regex_uses_the_regex_engine() {
        check_cases(&[
            Case {
                label: "anchored regex hit",
                config: json!({"reject": ["/^BUY\\s+NOW\\b/"]}),
                activity: note("BUY NOW while stocks last", "", ""),
                expected: Err(REJECT_REASON.to_owned()),
            },
            Case {
                label: "the anchor refuses a mid-text hit",
                config: json!({"reject": ["/^BUY\\s+NOW\\b/"]}),
                activity: note("please do not BUY NOW", "", ""),
                expected: Ok(note("please do not BUY NOW", "", "")),
            },
            Case {
                label: "inline case-insensitive flag",
                config: json!({"reject": ["/\\bcheap\\b/i"]}),
                activity: note("<p>a CHEAP watch</p>", "", ""),
                expected: Err(REJECT_REASON.to_owned()),
            },
            Case {
                label: "no flags, no folding",
                config: json!({"reject": ["/\\bcheap\\b/"]}),
                activity: note("<p>a CHEAP watch</p>", "", ""),
                expected: Ok(note("<p>a CHEAP watch</p>", "", "")),
            },
            Case {
                label: "a regex a plain string would miss",
                config: json!({"reject": ["/a.c/"]}),
                activity: note("<p>abc</p>", "", ""),
                expected: Err(REJECT_REASON.to_owned()),
            },
            Case {
                label: "a slash that closes nothing is a plain string",
                config: json!({"reject": ["/only"]}),
                activity: note("<p>only a slash elsewhere</p>", "", ""),
                expected: Ok(note("<p>only a slash elsewhere</p>", "", "")),
            },
            Case {
                label: "an unknown flag keeps the entry a literal",
                config: json!({"reject": ["/abc/u"]}),
                activity: note("<p>abc</p>", "", ""),
                expected: Ok(note("<p>abc</p>", "", "")),
            },
            Case {
                label: "and the literal slash string matches itself",
                config: json!({"reject": ["/only"]}),
                activity: note("<p>/only</p>", "", ""),
                expected: Err(REJECT_REASON.to_owned()),
            },
        ]);
    }

    /// A pattern that does not compile — or stands above the size limit —
    /// is skipped with a warning: it refuses nothing, and the other
    /// entries of its list keep working.
    #[test]
    fn an_invalid_regex_entry_is_skipped_and_the_rest_stand() {
        assert!(
            Pattern::compile("/[unclosed/").is_none(),
            "an unterminated bracket class is a regex that cannot compile"
        );
        assert!(
            Pattern::compile("/a{2,1}/").is_none(),
            "an invalid bounded repetition cannot compile either"
        );
        check_cases(&[
            Case {
                label: "the skipped pattern governs nothing",
                config: json!({"reject": ["/[unclosed/", "/a{2,1}/"]}),
                activity: note("<p>[unclosed( a{2,1}</p>", "", ""),
                expected: Ok(note("<p>[unclosed( a{2,1}</p>", "", "")),
            },
            Case {
                label: "its valid sibling still refuses",
                config: json!({"reject": ["/[unclosed/", "keeper"]}),
                activity: note("<p>KEEPER of the regex</p>", "", ""),
                expected: Err(REJECT_REASON.to_owned()),
            },
            Case {
                label: "and a valid sibling's miss passes",
                config: json!({"reject": ["/[unclosed/", "keeper"]}),
                activity: note("<p>nothing here</p>", "", ""),
                expected: Ok(note("<p>nothing here</p>", "", "")),
            },
        ]);
    }

    /// `federated_timeline_removal`: the golden rewrite moves the public
    /// address out of `to` into `cc`; a direct note has no timeline to
    /// leave and stands.
    #[test]
    fn the_federated_timeline_removal_list_unlists_a_matching_note() {
        let lists = json!({"federated_timeline_removal": ["politics", "/\\breject\\b/i"]});
        check_cases(&[
            Case {
                label: "substring hit unlists",
                config: lists.clone(),
                activity: note("<p>hello</p>", "about POLITICS", ""),
                expected: Ok(delisted(note("<p>hello</p>", "about POLITICS", ""))),
            },
            Case {
                label: "regex hit unlists",
                config: lists.clone(),
                activity: note("<p>please ReJect the bill</p>", "", ""),
                expected: Ok(delisted(note("<p>please ReJect the bill</p>", "", ""))),
            },
            Case {
                label: "no hit stands",
                config: lists.clone(),
                activity: note("<p>hello</p>", "", ""),
                expected: Ok(note("<p>hello</p>", "", "")),
            },
        ]);

        let direct = json!({
            "id": "https://peer.example/activities/2",
            "type": "Create",
            "actor": ALICE,
            "to": ["https://remote.example/users/bob"],
            "object": { "id": NOTE, "type": "Note", "to": ["https://remote.example/users/bob"], "content": "politics" },
        });
        assert_eq!(
            filtered(&lists, direct.clone()),
            Ok(direct),
            "a note nobody public was addressed keeps its recipients"
        );
    }

    /// `replace`: the golden rewrites — every entry applies to every text
    /// field in order, all matches go, an empty replacement deletes, a
    /// regex entry rewrites with its own flags, a `$` in the replacement
    /// stands as the literal it was written as, and malformed pairs drop
    /// out beside the valid ones.
    #[test]
    fn the_replace_list_rewrites_every_text_field() {
        check_cases(&[
            Case {
                label: "substring replaced in all three fields, everywhere",
                config: json!({"replace": [["foo", "bar"]]}),
                activity: note("<p>foo and FOO</p>", "foo", "foo?"),
                expected: Ok(note("<p>bar and bar</p>", "bar", "bar?")),
            },
            Case {
                label: "entries fold in configured order",
                config: json!({"replace": [["cat", "dog"], ["dog", "wolf"]]}),
                activity: note("<p>cat</p>", "", ""),
                expected: Ok(note("<p>wolf</p>", "", "")),
            },
            Case {
                label: "an empty replacement deletes",
                config: json!({"replace": [["bad word", ""]]}),
                activity: note("<p>a bad word here</p>", "", ""),
                expected: Ok(note("<p>a  here</p>", "", "")),
            },
            Case {
                label: "a regex entry rewrites its matches",
                config: json!({"replace": [["/\\bfoo\\b/i", "f*o"]]}),
                activity: note("<p>foo, foo! food</p>", "", ""),
                expected: Ok(note("<p>f*o, f*o! food</p>", "", "")),
            },
            Case {
                label: "a $ replacement is literal",
                config: json!({"replace": [["x", "$0$nope"]]}),
                activity: note("<p>axb</p>", "", ""),
                expected: Ok(note("<p>a$0$nopeb</p>", "", "")),
            },
            Case {
                label: "malformed pairs are skipped, the valid one stands",
                config: json!({"replace": [["orphan"], ["bad", "pair", "extra"], 7, ["ok", "fine"]]}),
                activity: note("<p>ok</p>", "", ""),
                expected: Ok(note("<p>fine</p>", "", "")),
            },
            Case {
                label: "an uncompilable regex pair is skipped",
                config: json!({"replace": [["/[unclosed/", "gone"], ["two", "2"]]}),
                activity: note("<p>two [unclosed/</p>", "", ""),
                expected: Ok(note("<p>2 [unclosed/</p>", "", "")),
            },
        ]);
    }

    /// The three lists weigh one another in Pleroma's order: the rejection
    /// and the delist read the text before any replacement has rewritten
    /// it, and what `replace` makes of the text cannot re-trigger them.
    #[test]
    fn reject_and_delist_are_weighed_before_replace_runs() {
        check_cases(&[
            Case {
                label: "a rejected word cannot be replaced away",
                config: json!({
                    "reject": ["spam"],
                    "replace": [["spam", "ham"]],
                }),
                activity: note("<p>spam</p>", "", ""),
                expected: Err(REJECT_REASON.to_owned()),
            },
            Case {
                label: "a replacement that would match the delist list does not",
                config: json!({
                    "federated_timeline_removal": ["politics"],
                    "replace": [["news", "politics"]],
                }),
                activity: note("<p>news</p>", "", ""),
                expected: Ok(note("<p>politics</p>", "", "")),
            },
        ]);
    }

    /// An `Update` is weighed exactly like a `Create`.
    #[test]
    fn an_update_is_weighed_like_a_create() {
        assert_eq!(
            filtered(&json!({"reject": ["edit"]}), updated("edited")),
            Err(REJECT_REASON.to_owned()),
            "the rejection reads an edited note"
        );
        assert_eq!(
            filtered(
                &json!({"replace": [["edited", "fixed"]]}),
                updated("edited")
            ),
            Ok(updated("fixed")),
            "and the replacement rewrites it"
        );
    }

    /// Everything outside the note shape passes untouched, lists armed or
    /// not: an activity with no embedded object, a non-note object, and a
    /// type the policy does not speak about.
    #[test]
    fn documents_without_an_embedded_note_pass_unchanged() {
        let armed = json!({"reject": ["x"]});
        for activity in [
            json!({
                "id": "https://peer.example/activities/3",
                "type": "Follow",
                "actor": ALICE,
                "object": "https://test.example/users/bob",
            }),
            json!({
                "id": "https://peer.example/activities/4",
                "type": "Create",
                "actor": ALICE,
                "object": NOTE,
            }),
            json!({
                "id": "https://peer.example/activities/5",
                "type": "Create",
                "actor": ALICE,
                "to": [PUBLIC],
                "object": { "id": NOTE, "type": "Profile", "content": "x" },
            }),
        ] {
            assert_eq!(
                filtered(&armed, activity.clone()),
                Ok(activity),
                "the reject list has nothing to read here"
            );
        }
    }

    /// The empty configuration is the no-op: no key, an empty object,
    /// empty lists and lists of unusable entries all hand every shape the
    /// policy speaks about straight through.
    #[test]
    fn an_empty_configuration_governs_nothing() {
        let shapes = [
            note("<p>free money politics foo</p>", "foo", "bar"),
            updated("anything"),
        ];
        for config in [
            None,
            Some(json!({})),
            Some(json!({"reject": [], "federated_timeline_removal": [], "replace": []})),
            Some(json!({"reject": "not-an-array", "replace": {"k": "v"}})),
            Some(json!({"nonsense": ["free money"]})),
        ] {
            let run = policy(config.as_ref());
            for activity in &shapes {
                assert_eq!(
                    run.filter(&ctx(), activity.clone()),
                    Ok(activity.clone()),
                    "config {config:?} is inert"
                );
            }
            assert_eq!(
                Value::Object(run.describe(&Transparency::open())),
                json!({"mrf_keyword": config.unwrap_or_else(|| json!({}))}),
                "and publishes the block it was given, or nothing, under its own key"
            );
        }
    }

    /// A non-object block is warned about, governs nothing, and describes
    /// nothing: there is no configuration subset to publish.
    #[test]
    fn a_non_object_block_governs_and_describes_nothing() {
        for params in [json!("nope"), json!(7), json!([{"reject": []}])] {
            let run = policy(Some(&params));
            let activity = note("<p>free money</p>", "politics", "foo");
            assert_eq!(
                run.filter(&ctx(), activity.clone()),
                Ok(activity),
                "{params} is not a block, so it enforces nothing"
            );
            assert_eq!(
                Value::Object(run.describe(&Transparency::open())),
                json!({"mrf_keyword": {}}),
                "{params} is not a subset to echo"
            );
        }
    }

    /// The policy is Pleroma's `KeywordPolicy`: it names itself, and the
    /// configured block comes back through `describe` verbatim under its
    /// `mrf_keyword` key — the patterns are strings as written, so
    /// transparency shows exactly what governs.
    #[test]
    fn the_policy_names_itself_and_describes_its_config() {
        assert_eq!(policy(None).name(), "KeywordPolicy");
        assert_eq!(
            Value::Object(policy(None).describe(&Transparency::open())),
            json!({"mrf_keyword": {}}),
            "no key, nothing to describe but its own name"
        );
        let block = json!({
            "reject": ["word", "/regex/"],
            "federated_timeline_removal": ["quiet"],
            "replace": [["a", "b"]],
        });
        assert_eq!(
            Value::Object(policy(Some(&block)).describe(&Transparency::open())),
            json!({"mrf_keyword": block}),
            "the configured subset comes back verbatim under the policy's key"
        );
    }

    /// [`List`]: the configuration keys round-trip, and anything else
    /// names no list.
    #[test]
    fn the_lists_round_trip_their_keys() {
        for list in List::ALL {
            assert_eq!(
                List::from_key(list.key()),
                Some(list),
                "round-trips: {list:?}"
            );
        }
        assert_eq!(List::from_key("nonsense"), None);
        assert_eq!(List::from_key(""), None);
    }
}
