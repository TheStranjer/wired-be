//! Pleroma's `HellthreadPolicy`: the mention-count gate of the
//! [pipeline](super).
//!
//! The reference is `lib/pleroma/web/activity_pub/mrf/hellthread_policy.ex`
//! — "block activities with too much mentions (configurable)". A note that
//! @-mentions a crowd is noise everywhere and a hellthread at scale, so the
//! two thresholds under the `hellthread` key of the `mrf` instance setting
//! weigh the crowd and answer:
//!
//! ```json
//! {"policies": ["hellthread"],
//!  "hellthread": {"delist_threshold": 10, "reject_threshold": 20}}
//! ```
//!
//! The count is the number of mention targets in the activity's `to` and
//! `cc` together, after dropping the public address and every
//! `/followers` collection: those two address the audience a note already
//! has, not people the author dragged in. The count is compared strictly —
//! Pleroma's `>` — so a threshold of 10 delists the eleventh mention:
//!
//! * over `reject_threshold` (default [`DEFAULT_REJECT_THRESHOLD`]): the
//!   activity is refused.
//! * over `delist_threshold` (default [`DEFAULT_DELIST_THRESHOLD`]): the
//!   public address moves from `to` into `cc` — the note arrives unlisted,
//!   and the crowd's home feeds never see it.
//!
//! Either stage is switched off by a threshold of 0, exactly as Pleroma's
//! `when threshold > 0` guards switch it off. Like the
//! [`simple`](super::simple) policy's addressing rules the rewrite applies
//! to the activity and its embedded note alike; the count itself reads the
//! activity's own addressing, which is where a delivering peer spells the
//! thread's recipients. A non-`Note`-family object and any other activity
//! type pass untouched.

use serde_json::{Map, Value, json};
use tracing::warn;

use crate::federation::{
    json,
    mrf::{
        MrfContext, MrfPolicy, MrfResult, Transparency,
        simple::{
            ADDRESS_FIELDS, FOLLOWERS_SUFFIX, is_note_activity, rewrite_each_document,
            unlist_document,
        },
        single_block,
    },
    note_parser::PUBLIC_RECIPIENTS,
};

/// The key of this policy's parameters inside the `mrf` instance setting,
/// and the name [`build_chain`](super::build_chain) resolves.
pub const PARAMS_KEY: &str = "hellthread";
/// The default number of mentions after which a note is unlisted —
/// Pleroma's documented suggestion.
pub const DEFAULT_DELIST_THRESHOLD: usize = 10;
/// The default number of mentions after which a note is refused —
/// Pleroma's documented suggestion.
pub const DEFAULT_REJECT_THRESHOLD: usize = 20;

/// The mentioned recipients of the activity's own `to` and `cc`: the count
/// both thresholds weigh. The public address and every `/followers`
/// collection are not mentions — they are the audiences the author posts
/// into anyway.
fn mentioned_recipients(activity: &Value) -> usize {
    ADDRESS_FIELDS
        .iter()
        .flat_map(|field| activity.get(*field).map_or_else(Vec::new, json::references))
        .filter(|address| !is_public_address(address) && !is_followers_collection(address))
        .count()
}

/// Whether an addressing entry is the public address in any of the
/// [`PUBLIC_RECIPIENTS`] spellings.
fn is_public_address(address: &str) -> bool {
    PUBLIC_RECIPIENTS.contains(&address)
}

/// Whether an addressing entry names a followers collection, by the
/// `{actor}/followers` URL convention every federation stack uses.
fn is_followers_collection(address: &str) -> bool {
    address.ends_with(FOLLOWERS_SUFFIX)
}

/// One configured threshold: its value when the block carries a whole
/// number under `key`, `default` when the key is absent, and `default`
/// (beside the warning that says why) when it holds anything else.
fn threshold(fields: Option<&Map<String, Value>>, key: &str, default: usize) -> usize {
    let Some(value) = fields.and_then(|fields| fields.get(key)) else {
        return default;
    };
    value
        .as_u64()
        .and_then(|count| usize::try_from(count).ok())
        .unwrap_or_else(|| {
            warn!(
                key,
                "an `mrf.hellthread` threshold is not a whole number; its default applies"
            );
            default
        })
}

/// The mention-count gate: Pleroma's `MRF.HellthreadPolicy`.
#[derive(Clone, Debug)]
pub struct HellthreadPolicy {
    /// The parameter block as configured, for
    /// [`describe`](MrfPolicy::describe); `None` when the setting carries
    /// none or a non-object.
    params: Option<Value>,
    /// The mention count above which the note is unlisted; 0 disables.
    delist_threshold: usize,
    /// The mention count above which the note is refused; 0 disables.
    reject_threshold: usize,
}

impl HellthreadPolicy {
    /// The policy over the `hellthread` key of the `mrf` setting
    /// (`None` when the setting carries none): the two configured
    /// thresholds, each defaulting when its key is absent or unusable.
    #[must_use]
    pub fn new(params: Option<Value>) -> Self {
        let fields = params.as_ref().and_then(Value::as_object);
        if params.is_some() && fields.is_none() {
            warn!(
                "the `hellthread` MRF setting is not a JSON object; \
                 the policy runs on its default thresholds"
            );
        }
        let delist_threshold = threshold(fields, "delist_threshold", DEFAULT_DELIST_THRESHOLD);
        let reject_threshold = threshold(fields, "reject_threshold", DEFAULT_REJECT_THRESHOLD);
        Self {
            params: params.filter(Value::is_object),
            delist_threshold,
            reject_threshold,
        }
    }

    /// Whether `threshold` is in force and `mentions` is over it —
    /// Pleroma's strict `>`, with 0 as the off switch.
    const fn over(mentions: usize, threshold: usize) -> bool {
        threshold > 0 && mentions > threshold
    }
}

impl MrfPolicy for HellthreadPolicy {
    fn name(&self) -> &'static str {
        "HellthreadPolicy"
    }

    fn filter(&self, _ctx: &MrfContext, activity: Value) -> MrfResult {
        if !is_note_activity(&activity) {
            return Ok(activity);
        }
        let mentions = mentioned_recipients(&activity);
        if Self::over(mentions, self.reject_threshold) {
            return Err(format!(
                "[HellthreadPolicy] {mentions} recipients is over the limit of {}",
                self.reject_threshold
            ));
        }
        let mut activity = activity;
        if Self::over(mentions, self.delist_threshold) {
            rewrite_each_document(&mut activity, unlist_document);
        }
        Ok(activity)
    }

    fn describe(&self, _transparency: &Transparency) -> Map<String, Value> {
        single_block(
            self.name(),
            self.params.clone().unwrap_or_else(|| json!({})),
        )
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use std::sync::Arc;

    use super::*;
    use crate::federation::mrf::{
        MrfConfig,
        simple::{PUBLIC_ADDRESS, followers_collection},
    };

    /// The author the fixtures post as.
    const ALICE: &str = "https://peer.example/users/alice";
    /// The public address, full `ActivityStreams` spelling.
    const PUBLIC: &str = PUBLIC_ADDRESS;
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

    /// A public `Create` of a note by alice addressing `mentions` further
    /// recipients: `to` carries the public address, `cc` the author's
    /// followers collection and every mention.
    fn create_with(mentions: usize) -> Value {
        let cc: Vec<Value> = std::iter::once(json!(followers_collection_of()))
            .chain((0..mentions).map(|n| json!(format!("https://remote.example/users/{n}"))))
            .collect();
        json!({
            "id": "https://peer.example/activities/1",
            "type": "Create",
            "actor": ALICE,
            "to": [PUBLIC],
            "cc": cc,
            "object": {
                "id": NOTE,
                "type": "Note",
                "attributedTo": ALICE,
                "to": [PUBLIC],
                "cc": cc,
                "content": "<p>hello</p>",
            },
        })
    }

    /// The fixture author's followers collection, by the convention the
    /// policy falls back to.
    fn followers_collection_of() -> String {
        followers_collection(&json!({"actor": ALICE}))
    }

    /// The delisted golden of [`create_with`]: the public address out of
    /// `to` and onto the end of `cc`, on both documents; the mentions stay
    /// where they were.
    fn delisted(mentions: usize) -> Value {
        let mut expected = create_with(mentions);
        expected["to"] = json!([]);
        expected["cc"]
            .as_array_mut()
            .expect("the fixture addresses an array")
            .push(json!(PUBLIC));
        expected["object"]["to"] = json!([]);
        expected["object"]["cc"]
            .as_array_mut()
            .expect("the fixture note addresses an array")
            .push(json!(PUBLIC));
        expected
    }

    /// The table: one configured block, one mention count, one golden
    /// answer at that count.
    struct Case {
        /// What the case is about, for the assertion message.
        label: &'static str,
        /// The `hellthread` block under test.
        config: Value,
        /// How many mentions the fixture carries.
        mentions: usize,
        /// The golden answer: the accepted document or the rejection.
        expected: MrfResult,
    }

    /// Runs the table, one golden JSON answer per case.
    fn check_cases(cases: &[Case]) {
        for case in cases {
            let policy = HellthreadPolicy::new(Some(case.config.clone()));
            assert_eq!(
                policy.filter(&ctx(), create_with(case.mentions)),
                case.expected.clone(),
                "case: {}",
                case.label
            );
        }
    }

    /// The default thresholds — delist at 10, reject at 20, compared
    /// strictly: the boundary count stands, one mention more tips the
    /// note into its answer.
    #[test]
    fn the_default_thresholds_delist_at_eleven_and_reject_at_twenty_one() {
        check_cases(&[
            Case {
                label: "at the delist boundary",
                config: json!({}),
                mentions: DEFAULT_DELIST_THRESHOLD,
                expected: Ok(create_with(DEFAULT_DELIST_THRESHOLD)),
            },
            Case {
                label: "one over the delist boundary",
                config: json!({}),
                mentions: DEFAULT_DELIST_THRESHOLD + 1,
                expected: Ok(delisted(DEFAULT_DELIST_THRESHOLD + 1)),
            },
            Case {
                label: "at the reject boundary (already delisted)",
                config: json!({}),
                mentions: DEFAULT_REJECT_THRESHOLD,
                expected: Ok(delisted(DEFAULT_REJECT_THRESHOLD)),
            },
            Case {
                label: "one over the reject boundary",
                config: json!({}),
                mentions: DEFAULT_REJECT_THRESHOLD + 1,
                expected: Err(format!(
                    "[HellthreadPolicy] {} recipients is over the limit of {DEFAULT_REJECT_THRESHOLD}",
                    DEFAULT_REJECT_THRESHOLD + 1
                )),
            },
        ]);
    }

    /// Configured thresholds are honored: `delist_threshold` and
    /// `reject_threshold` read from the block, and any whole number works.
    #[test]
    fn configured_thresholds_move_the_boundaries() {
        let config = json!({"delist_threshold": 2, "reject_threshold": 3});
        check_cases(&[
            Case {
                label: "under both",
                config: config.clone(),
                mentions: 2,
                expected: Ok(create_with(2)),
            },
            Case {
                label: "over the delist",
                config: config.clone(),
                mentions: 3,
                expected: Ok(delisted(3)),
            },
            Case {
                label: "over the reject",
                config,
                mentions: 4,
                expected: Err("[HellthreadPolicy] 4 recipients is over the limit of 3".to_owned()),
            },
        ]);
    }

    /// A threshold of 0 switches its stage off, Pleroma's documented off
    /// switch: zero rejects delist on the delist threshold, and zero
    /// delists hand a crowd back untouched.
    #[test]
    fn a_zero_threshold_disables_its_stage() {
        check_cases(&[
            Case {
                label: "reject off, delist still on",
                config: json!({"reject_threshold": 0}),
                mentions: 100,
                expected: Ok(delisted(100)),
            },
            Case {
                label: "delist off, reject still on",
                config: json!({"delist_threshold": 0}),
                mentions: 11,
                expected: Ok(create_with(11)),
            },
            Case {
                label: "both off",
                config: json!({"delist_threshold": 0, "reject_threshold": 0}),
                mentions: 100,
                expected: Ok(create_with(100)),
            },
        ]);
    }

    /// The counting rule: the public address in any spelling and every
    /// `/followers` collection stay out of the count — of the activity
    /// itself, in `to` as in `cc` — and every other recipient counts.
    #[test]
    fn public_addresses_and_followers_collections_are_not_mentions() {
        let crowd: Vec<Value> = (0..8)
            .map(|n| json!(format!("https://remote.example/users/{n}")))
            .collect();
        let with_short_public = json!({
            "id": "https://peer.example/activities/2",
            "type": "Create",
            "actor": ALICE,
            "to": ["as:Public", "https://peer.example/users/alice/followers"],
            "cc": crowd,
            "object": {
                "id": NOTE,
                "type": "Note",
                "to": ["Public"],
                "cc": ["https://other.example/users/alice/followers"],
            },
        });
        // 8 mentions count, the three public spellings and the two
        // followers collections (this author's and any other's) do not:
        // over a delist threshold of 7, the note unlists; at 8, it stands.
        check_cases(&[Case {
            label: "the crowd is the mentions, not the audiences",
            config: json!({"delist_threshold": 7, "reject_threshold": 0}),
            mentions: 8,
            expected: Ok(delisted(8)),
        }]);
        let policy =
            HellthreadPolicy::new(Some(json!({"delist_threshold": 8, "reject_threshold": 0})));
        assert_eq!(
            policy.filter(&ctx(), with_short_public.clone()),
            Ok(with_short_public),
            "the same fixture stands at a threshold of 8: the audiences were not counted"
        );
    }

    /// A non-public note over the reject threshold is still refused; the
    /// delist rewrite simply has nothing to move.
    #[test]
    fn a_direct_crowd_is_refused_and_never_rewritten_by_the_delist() {
        let direct = json!({
            "id": "https://peer.example/activities/3",
            "type": "Create",
            "actor": ALICE,
            "to": ["https://remote.example/users/bob", "https://remote.example/users/carol"],
            "object": { "id": NOTE, "type": "Note", "to": ["https://remote.example/users/bob"] },
        });
        let policy = HellthreadPolicy::new(Some(json!({"reject_threshold": 1})));
        assert_eq!(
            policy.filter(&ctx(), direct.clone()),
            Err("[HellthreadPolicy] 2 recipients is over the limit of 1".to_owned()),
            "reject weighs the mention count whatever the activity addresses"
        );
        let delister =
            HellthreadPolicy::new(Some(json!({"delist_threshold": 1, "reject_threshold": 0})));
        assert_eq!(
            delister.filter(&ctx(), direct.clone()),
            Ok(direct),
            "and the delist has no public address to move"
        );
    }

    /// Everything outside the note shape passes at any count: an
    /// `Update`, a non-note object, an embedded-less object, a `Follow`.
    #[test]
    fn other_activities_pass_at_any_count() {
        let strict = json!({"delist_threshold": 0, "reject_threshold": 1});
        let crowd: Vec<Value> = (0..5)
            .map(|n| json!(format!("https://remote.example/users/{n}")))
            .collect();
        for activity in [
            json!({
                "id": "https://peer.example/activities/4",
                "type": "Announce",
                "actor": ALICE,
                "to": [PUBLIC],
                "cc": crowd,
                "object": NOTE,
            }),
            json!({
                "id": "https://peer.example/activities/5",
                "type": "Create",
                "actor": ALICE,
                "to": [PUBLIC],
                "cc": crowd,
                "object": { "id": NOTE, "type": "Profile", "content": "hi" },
            }),
            json!({
                "id": "https://peer.example/activities/6",
                "type": "Follow",
                "actor": ALICE,
                "object": "https://test.example/users/bob",
            }),
        ] {
            let config = strict.clone();
            let policy = HellthreadPolicy::new(Some(config));
            assert_eq!(
                policy.filter(&ctx(), activity.clone()),
                Ok(activity),
                "the gate speaks about note Creates"
            );
        }
    }

    /// The empty configuration is not the off switch — it is the defaults:
    /// no key, an empty object and unusable values all leave 10 and 20 in
    /// force, with the boundary behavior [`the_default_thresholds_delist_at_eleven_and_reject_at_twenty_one`] checks.
    #[test]
    fn missing_or_unusable_thresholds_fall_back_to_the_defaults() {
        for config in [
            json!({}),
            json!({"delist_threshold": "many"}),
            json!({"delist_threshold": -1, "reject_threshold": null}),
            json!({"reject_threshold": {"how": "many"}}),
        ] {
            let policy = HellthreadPolicy::new(Some(config.clone()));
            assert_eq!(
                policy.delist_threshold, DEFAULT_DELIST_THRESHOLD,
                "{config} leaves the delist default"
            );
            assert_eq!(
                policy.reject_threshold, DEFAULT_REJECT_THRESHOLD,
                "{config} leaves the reject default"
            );
        }
        // And a policy over no parameters at all runs the same default
        // gate: 11 mentions delist, nothing passes reject.
        let policy = HellthreadPolicy::new(None);
        assert_eq!(
            policy.filter(&ctx(), create_with(DEFAULT_DELIST_THRESHOLD + 1)),
            Ok(delisted(DEFAULT_DELIST_THRESHOLD + 1)),
            "no key is the documented default configuration"
        );
    }

    /// The policy is Pleroma's `HellthreadPolicy`: it names itself and
    /// publishes the subset it was configured with as its one
    /// `mrf_hellthread` object — the empty object when nothing usable was.
    #[test]
    fn the_policy_names_itself_and_describes_its_config() {
        assert_eq!(HellthreadPolicy::new(None).name(), "HellthreadPolicy");
        assert_eq!(
            Value::Object(HellthreadPolicy::new(None).describe(&Transparency::open())),
            json!({"mrf_hellthread": {}}),
            "no key, nothing to describe but its own name"
        );
        assert_eq!(
            Value::Object(
                HellthreadPolicy::new(Some(json!("nope"))).describe(&Transparency::open())
            ),
            json!({"mrf_hellthread": {}}),
            "a non-object block is no subset to echo"
        );
        let block = json!({"delist_threshold": 5, "reject_threshold": 15});
        assert_eq!(
            Value::Object(
                HellthreadPolicy::new(Some(block.clone())).describe(&Transparency::open())
            ),
            json!({"mrf_hellthread": block}),
            "the configured subset comes back verbatim under the policy's key"
        );
    }
}
