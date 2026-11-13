//! Pleroma's `ObjectAgePolicy`: the freshness gate of the [pipeline](super).
//!
//! The reference is `lib/pleroma/web/activity_pub/mrf/object_age_policy.ex`
//! — "filter activities depending on their age": a `Create` whose embedded
//! object `published` an unreasonably long time ago is what resurrectors
//! and timestamp-spoofers deliver, and the two keys under the `object_age`
//! name of the `mrf` instance setting say what to do with one:
//!
//! ```json
//! {"policies": ["object_age"],
//!  "object_age": {"threshold": 604800, "actions": ["delist", "reject"]}}
//! ```
//!
//! An object counts as too old when `now - published` exceeds
//! [`DEFAULT_THRESHOLD_SECS`] (or the configured `threshold`) in seconds —
//! Pleroma's strict `>`, against the chain's single shared
//! [`now`](MrfContext::now), so two age checks inside one activity cannot
//! disagree. A `published` the policy cannot read (absent, not a string,
//! not an RFC 3339 instant) is no age at all, and the activity passes; a
//! future timestamp is never old. The `actions` list names what falls on
//! an activity that is old:
//!
//! * [`Action::Reject`] refuses it outright — the first action weighed, so
//!   nothing rewrites a document that is about to be dropped.
//! * [`Action::Delist`] moves the public address from `to` into `cc` —
//!   the note arrives unlisted, the same rewrite the
//!   [`simple`](super::simple) policy's `federated_timeline_removal` list
//!   performs.
//! * [`Action::StripFollowers`] takes the author's followers collection
//!   out of `to` and `cc`, so the post reaches nobody's home feed by
//!   audience address either.
//!
//! Both rewrites apply to the activity and its embedded object alike, as
//! everywhere else in the chain. An empty or absent `actions` list leaves
//! even a prehistoric post standing; unknown action names are warned
//! about and skipped.

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use tracing::warn;

use crate::{
    federation::mrf::{
        MrfContext, MrfPolicy, MrfResult,
        simple::{
            activity_type, followers_collection, remove_reference, rewrite_each_document,
            unlist_document,
        },
    },
    util::time::parse_time,
};

/// The key of this policy's parameters inside the `mrf` instance setting,
/// and the name [`build_chain`](super::build_chain) resolves.
pub const PARAMS_KEY: &str = "object_age";
/// The default age in seconds a post may have before the actions fall on
/// it: one week, Pleroma's configured default.
pub const DEFAULT_THRESHOLD_SECS: i64 = 604_800;
/// The one activity type this policy speaks about.
const CREATE_TYPE: &str = "Create";

/// One of the actions the policy takes on a too-old `Create`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// `delist`: the public address moves from `to` into `cc`.
    Delist,
    /// `strip_followers`: the author's followers collection leaves `to`
    /// and `cc`.
    StripFollowers,
    /// `reject`: the activity is refused.
    Reject,
}

impl Action {
    /// Every action, in the order Pleroma documents the configuration.
    pub const ALL: [Self; 3] = [Self::Delist, Self::StripFollowers, Self::Reject];

    /// The name the action is configured under.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Delist => "delist",
            Self::StripFollowers => "strip_followers",
            Self::Reject => "reject",
        }
    }

    /// The action a configured name selects, `None` for anything else.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.key() == key)
    }
}

/// The instant the embedded object of a `Create` says it was `published`,
/// `None` for any other shape or an unreadable timestamp: the age the gate
/// weighs, read straight off the document as Pleroma does.
fn create_published(activity: &Value) -> Option<DateTime<Utc>> {
    let created = activity_type(activity).is_some_and(|kind| kind == CREATE_TYPE);
    if !created {
        return None;
    }
    activity
        .get("object")
        .and_then(Value::as_object)
        .and_then(|object| object.get("published"))
        .and_then(Value::as_str)
        .and_then(parse_time)
}

/// How many seconds past [`MrfContext::now`] `published` sits; negative
/// for a timestamp from the future, which no threshold configured as a
/// positive age can ever cross.
fn age_secs(now: DateTime<Utc>, published: DateTime<Utc>) -> i64 {
    (now - published).num_seconds()
}

/// The configured actions, in configured order: every recognizable name,
/// while anything unrecognizable warns and drops out.
fn configured_actions(fields: Option<&Map<String, Value>>) -> Vec<Action> {
    let Some(value) = fields.and_then(|fields| fields.get("actions")) else {
        return Vec::new();
    };
    let Value::Array(entries) = value else {
        warn!("the `actions` of the `object_age` MRF setting is not an array; no action runs");
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let Some(name) = entry.as_str() else {
                warn!(
                    entry = %entry,
                    "an `mrf.object_age` action is not a string; ignored"
                );
                return None;
            };
            let Some(action) = Action::from_key(name) else {
                warn!(
                    action = name,
                    "an unknown `mrf.object_age` action was configured; ignored"
                );
                return None;
            };
            Some(action)
        })
        .collect()
}

/// The configured threshold in seconds, [`DEFAULT_THRESHOLD_SECS`] when
/// the key is absent and (beside the warning that says why) when it holds
/// anything but a whole number.
fn configured_threshold(fields: Option<&Map<String, Value>>) -> i64 {
    let Some(value) = fields.and_then(|fields| fields.get("threshold")) else {
        return DEFAULT_THRESHOLD_SECS;
    };
    value.as_i64().unwrap_or_else(|| {
        warn!(
            threshold = %value,
            "the `threshold` of the `object_age` MRF setting is not a whole number; the default applies"
        );
        DEFAULT_THRESHOLD_SECS
    })
}

/// The age gate: Pleroma's `MRF.ObjectAgePolicy`, the chain stage that
/// refuses, unlists or strips the audience of a resurrected post.
#[derive(Clone, Debug)]
pub struct ObjectAgePolicy {
    /// The parameter block as configured, for
    /// [`describe`](MrfPolicy::describe); `None` when the setting carries
    /// none or a non-object.
    params: Option<Value>,
    /// The age in seconds a post may have before the actions fall on it.
    threshold_secs: i64,
    /// What falls on it, in configured order.
    actions: Vec<Action>,
}

impl ObjectAgePolicy {
    /// The policy over the `object_age` key of the `mrf` setting (`None`
    /// when the setting carries none): the configured threshold and action
    /// list, each defaulting to nothing-and-quiet when absent or unusable.
    #[must_use]
    pub fn new(params: Option<Value>) -> Self {
        let fields = params.as_ref().and_then(Value::as_object);
        if params.is_some() && fields.is_none() {
            warn!(
                "the `object_age` MRF setting is not a JSON object; \
                 the policy runs with no actions"
            );
        }
        let threshold_secs = configured_threshold(fields);
        let actions = configured_actions(fields);
        Self {
            params: params.filter(Value::is_object),
            threshold_secs,
            actions,
        }
    }

    /// Whether `action` is one of the configured ones.
    fn armed(&self, action: Action) -> bool {
        self.actions.contains(&action)
    }
}

/// Applies one in-force rewriting action to `activity`; the reject is
/// weighed by the filter itself, before anything rewrites.
fn apply_action(action: Action, activity: &mut Value) {
    match action {
        Action::Delist => rewrite_each_document(activity, unlist_document),
        Action::StripFollowers => {
            let followers = followers_collection(activity);
            rewrite_each_document(activity, |document| {
                remove_reference(document, "to", &followers);
                remove_reference(document, "cc", &followers);
            });
        }
        Action::Reject => {}
    }
}

impl MrfPolicy for ObjectAgePolicy {
    fn name(&self) -> &'static str {
        "ObjectAgePolicy"
    }

    fn filter(&self, ctx: &MrfContext, activity: Value) -> MrfResult {
        let Some(published) = create_published(&activity) else {
            return Ok(activity);
        };
        let age = age_secs(ctx.now, published);
        if age <= self.threshold_secs {
            return Ok(activity);
        }
        if self.armed(Action::Reject) {
            return Err(format!(
                "[ObjectAgePolicy] the object is {} seconds past its {} second threshold",
                age - self.threshold_secs,
                self.threshold_secs
            ));
        }
        let mut activity = activity;
        for action in [Action::Delist, Action::StripFollowers] {
            if self.armed(action) {
                apply_action(action, &mut activity);
            }
        }
        Ok(activity)
    }

    fn describe(&self) -> Value {
        self.params.clone().unwrap_or_else(|| json!({}))
    }
}

#[cfg(test)]
mod tests {
    use chrono::{SecondsFormat, TimeDelta, TimeZone as _};
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
    /// The author's followers collection, the `/followers` convention.
    const FOLLOWERS: &str = "https://peer.example/users/alice/followers";
    /// The note a fixture `Create` announces.
    const NOTE: &str = "https://peer.example/objects/1";
    /// A second recipient the fixtures keep in `cc`, beside the followers.
    const BOB: &str = "https://remote.example/users/bob";

    /// The instant every age test weighs against: 2026-11-11T12:00:00Z.
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap()
    }

    /// The context of a run at a fixed [`now`].
    fn ctx() -> MrfContext {
        MrfContext {
            config: Arc::new(MrfConfig::assemble(
                vec![PARAMS_KEY.to_owned()],
                Map::from_iter([(PARAMS_KEY.to_owned(), json!({}))]),
            )),
            now: now(),
            local_domain: "test.example".to_owned(),
            author_follower_count: None,
            author_status_count: None,
            author_tags: Vec::new(),
            target_tags: Vec::new(),
        }
    }

    /// A public `Create` of a note published at the RFC 3339 `published`,
    /// by alice: `to` the public address, `cc` the followers and one
    /// mention, on the activity and the embedded object alike.
    fn create_published_at(published: &str) -> Value {
        json!({
            "id": "https://peer.example/activities/1",
            "type": "Create",
            "actor": ALICE,
            "to": [PUBLIC],
            "cc": [FOLLOWERS, BOB],
            "object": {
                "id": NOTE,
                "type": "Note",
                "attributedTo": ALICE,
                "to": [PUBLIC],
                "cc": [FOLLOWERS, BOB],
                "published": published,
                "content": "<p>hello</p>",
            },
        })
    }

    /// The RFC 3339 string `secs` seconds before [`now`].
    fn published_secs_ago(secs: i64) -> String {
        (now() - TimeDelta::seconds(secs)).to_rfc3339_opts(SecondsFormat::Secs, true)
    }

    /// The fixture `Create`, published `secs` seconds before [`now`].
    fn create_aged(secs: i64) -> Value {
        create_published_at(&published_secs_ago(secs))
    }

    /// The delisted golden of the aged fixture: Public out of `to`, onto
    /// the end of `cc`, on both documents.
    fn delisted_aged(secs: i64) -> Value {
        let mut expected = create_aged(secs);
        expected["to"] = json!([]);
        expected["cc"] = json!([FOLLOWERS, BOB, PUBLIC]);
        expected["object"]["to"] = json!([]);
        expected["object"]["cc"] = json!([FOLLOWERS, BOB, PUBLIC]);
        expected
    }

    /// The follower-stripped golden of the aged fixture: the followers
    /// collection out of both fields of both documents.
    fn stripped_aged(secs: i64) -> Value {
        let mut expected = create_aged(secs);
        expected["to"] = json!([PUBLIC]);
        expected["cc"] = json!([BOB]);
        expected["object"]["to"] = json!([PUBLIC]);
        expected["object"]["cc"] = json!([BOB]);
        expected
    }

    /// The table: one configured block, one fixture, one golden answer.
    struct Case {
        /// What the case is about, for the assertion message.
        label: &'static str,
        /// The `object_age` block under test.
        config: Value,
        /// The activity the chain hands the policy.
        activity: Value,
        /// The golden answer: the accepted document or the rejection.
        expected: MrfResult,
    }

    /// Runs the table, one golden JSON answer per case.
    fn check_cases(cases: &[Case]) {
        for case in cases {
            let policy = ObjectAgePolicy::new(Some(case.config.clone()));
            assert_eq!(
                policy.filter(&ctx(), case.activity.clone()),
                case.expected.clone(),
                "case: {}",
                case.label
            );
        }
    }

    /// Each configured action, on its own and together, at the golden
    /// in-and-out JSON: reject refuses, delist unlists, `strip_followers`
    /// takes the followers collection out of both fields, and a young
    /// post stands under every action armed.
    #[test]
    fn each_configured_action_answers_for_an_old_create() {
        check_cases(&[
            Case {
                label: "reject",
                config: json!({"threshold": 100, "actions": ["reject"]}),
                activity: create_aged(200),
                expected: Err(
                    "[ObjectAgePolicy] the object is 100 seconds past its 100 second threshold"
                        .to_owned(),
                ),
            },
            Case {
                label: "delist",
                config: json!({"threshold": 100, "actions": ["delist"]}),
                activity: create_aged(200),
                expected: Ok(delisted_aged(200)),
            },
            Case {
                label: "strip_followers",
                config: json!({"threshold": 100, "actions": ["strip_followers"]}),
                activity: create_aged(200),
                expected: Ok(stripped_aged(200)),
            },
            Case {
                label: "delist and strip together, configured order irrelevant",
                config: json!({
                    "threshold": 100,
                    "actions": ["strip_followers", "delist"],
                }),
                activity: create_aged(200),
                expected: Ok(json!({
                    "id": "https://peer.example/activities/1",
                    "type": "Create",
                    "actor": ALICE,
                    "to": [],
                    "cc": [BOB, PUBLIC],
                    "object": {
                        "id": NOTE,
                        "type": "Note",
                        "attributedTo": ALICE,
                        "to": [],
                        "cc": [BOB, PUBLIC],
                        "published": published_secs_ago(200),
                        "content": "<p>hello</p>",
                    },
                })),
            },
            Case {
                label: "young enough, even with every action armed",
                config: json!({
                    "threshold": 100,
                    "actions": ["reject", "delist", "strip_followers"],
                }),
                activity: create_aged(50),
                expected: Ok(create_aged(50)),
            },
        ]);
    }

    /// The age comparison is Pleroma's strict `>`: exactly `threshold`
    /// seconds of age stands, one second older falls under the actions,
    /// and a future timestamp is never old.
    #[test]
    fn the_threshold_is_compared_strictly() {
        check_cases(&[
            Case {
                label: "exactly at the threshold",
                config: json!({"threshold": 100, "actions": ["reject"]}),
                activity: create_aged(100),
                expected: Ok(create_aged(100)),
            },
            Case {
                label: "one second over it",
                config: json!({"threshold": 100, "actions": ["reject"]}),
                activity: create_aged(101),
                expected: Err(
                    "[ObjectAgePolicy] the object is 1 seconds past its 100 second threshold"
                        .to_owned(),
                ),
            },
            Case {
                label: "published in the future",
                config: json!({"threshold": 100, "actions": ["reject"]}),
                activity: create_aged(-60),
                expected: Ok(create_aged(-60)),
            },
        ]);
    }

    /// The default threshold is one week: an eight-day-old post falls
    /// under the actions with no `threshold` configured, a six-day-old
    /// one stands, and no `actions` at all is the no-op even for the
    /// eight-day-old.
    #[test]
    fn the_default_threshold_is_a_week_and_the_default_actions_are_none() {
        let eight_days = DEFAULT_THRESHOLD_SECS + 86_400;
        let six_days = DEFAULT_THRESHOLD_SECS - 86_400;
        check_cases(&[
            Case {
                label: "a configured week-old post is rejected",
                config: json!({"actions": ["reject"]}),
                activity: create_aged(eight_days),
                expected: Err(format!(
                    "[ObjectAgePolicy] the object is 86400 seconds past its {DEFAULT_THRESHOLD_SECS} second threshold"
                )),
            },
            Case {
                label: "a younger one stands",
                config: json!({"actions": ["reject"]}),
                activity: create_aged(six_days),
                expected: Ok(create_aged(six_days)),
            },
        ]);
        for config in [json!({}), json!({"threshold": 1})] {
            let policy = ObjectAgePolicy::new(Some(config));
            assert_eq!(
                policy.filter(&ctx(), create_aged(eight_days)),
                Ok(create_aged(eight_days)),
                "no actions configured, nothing runs"
            );
        }
    }

    /// A document the age check cannot read passes however armed the
    /// policy is: no embedded object, no `published`, a `published` that
    /// is not a string or not an instant, and any activity but a `Create`.
    #[test]
    fn unreadable_age_passes_an_armed_policy() {
        let armed = json!({"threshold": 0, "actions": ["reject"]});
        for activity in [
            json!({
                "id": "https://peer.example/activities/2",
                "type": "Create",
                "actor": ALICE,
                "object": NOTE,
            }),
            json!({
                "id": "https://peer.example/activities/3",
                "type": "Create",
                "actor": ALICE,
                "object": { "id": NOTE, "type": "Note", "content": "<p>hello</p>" },
            }),
            json!({
                "id": "https://peer.example/activities/4",
                "type": "Create",
                "actor": ALICE,
                "object": { "id": NOTE, "type": "Note", "published": "last tuesday" },
            }),
            json!({
                "id": "https://peer.example/activities/5",
                "type": "Create",
                "actor": ALICE,
                "object": { "id": NOTE, "type": "Note", "published": 12345 },
            }),
            json!({
                "id": "https://peer.example/activities/7",
                "type": "Announce",
                "actor": ALICE,
                "object": { "id": NOTE, "type": "Note", "published": published_secs_ago(9_999_999) },
            }),
            json!({
                "id": "https://peer.example/activities/8",
                "type": "Update",
                "actor": ALICE,
                "object": { "id": NOTE, "type": "Note", "published": published_secs_ago(9_999_999) },
            }),
        ] {
            let policy = ObjectAgePolicy::new(Some(armed.clone()));
            assert_eq!(
                policy.filter(&ctx(), activity.clone()),
                Ok(activity),
                "the age gate has nothing to read here"
            );
        }
    }

    /// Unusable configuration drops out entry by entry: an action name the
    /// policy does not know warns and runs nothing — the known ones
    /// beside it still run — and a non-numeric threshold falls back to the
    /// default.
    #[test]
    fn unusable_configuration_drops_out_and_the_usable_part_stands() {
        check_cases(&[
            Case {
                label: "the unknown action is ignored, the known one runs",
                config: json!({"threshold": 100, "actions": ["banish", "delist"]}),
                activity: create_aged(200),
                expected: Ok(delisted_aged(200)),
            },
            Case {
                label: "a non-string action is ignored too",
                config: json!({"threshold": 100, "actions": [7, "delist"]}),
                activity: create_aged(200),
                expected: Ok(delisted_aged(200)),
            },
            Case {
                label: "only unknown actions is no actions",
                config: json!({"threshold": 100, "actions": ["banish"]}),
                activity: create_aged(200),
                expected: Ok(create_aged(200)),
            },
            Case {
                label: "a non-array actions is no actions",
                config: json!({"threshold": 100, "actions": "reject"}),
                activity: create_aged(200),
                expected: Ok(create_aged(200)),
            },
            Case {
                label: "a non-numeric threshold falls back to a week",
                config: json!({"threshold": "soon", "actions": ["reject"]}),
                activity: create_aged(DEFAULT_THRESHOLD_SECS + 86_400),
                expected: Err(format!(
                    "[ObjectAgePolicy] the object is 86400 seconds past its {DEFAULT_THRESHOLD_SECS} second threshold"
                )),
            },
        ]);
    }

    /// The empty configuration is the no-op: no key, an empty object and
    /// a non-object document all hand an eight-day-old post straight
    /// through — nothing configured means nothing to run.
    #[test]
    fn an_empty_configuration_governs_nothing() {
        let old = create_aged(DEFAULT_THRESHOLD_SECS + 86_400);
        for config in [None, Some(json!({})), Some(json!("nope"))] {
            let policy = ObjectAgePolicy::new(config.clone());
            assert_eq!(
                policy.filter(&ctx(), old.clone()),
                Ok(old.clone()),
                "config {config:?} is inert"
            );
            let expected = match &config {
                Some(block @ Value::Object(_)) => block.clone(),
                _ => json!({}),
            };
            assert_eq!(
                policy.describe(),
                expected,
                "and describes the block it was given, or nothing"
            );
        }
    }

    /// The policy is Pleroma's `ObjectAgePolicy`: it names itself, knows
    /// its three action names, and describes back the subset it was
    /// configured with.
    #[test]
    fn the_policy_names_itself_and_describes_its_config() {
        assert_eq!(ObjectAgePolicy::new(None).name(), "ObjectAgePolicy");
        assert_eq!(
            ObjectAgePolicy::new(None).describe(),
            json!({}),
            "no key, nothing to describe"
        );
        let block = json!({"threshold": 3600, "actions": ["delist"]});
        assert_eq!(
            ObjectAgePolicy::new(Some(block.clone())).describe(),
            block,
            "the configured subset comes back verbatim"
        );
        for action in Action::ALL {
            assert_eq!(
                Action::from_key(action.key()),
                Some(action),
                "round-trips: {action:?}"
            );
        }
        assert_eq!(Action::from_key("banish"), None, "no such action");
    }

    /// The followers collection the strip acts on is the one the activity
    /// addresses when it names one on the author's own host, and the
    /// `{actor}/followers` convention otherwise: the same reading the
    /// [`simple`](super::simple) policy uses to put a collection address
    /// there.
    #[test]
    fn the_strip_followers_rewrite_targets_the_authors_own_collection() {
        let addressed = json!({
            "id": "https://peer.example/activities/9",
            "type": "Create",
            "actor": ALICE,
            "to": [PUBLIC, "https://peer.example/other/followers"],
            "object": {
                "id": NOTE,
                "type": "Note",
                "to": [PUBLIC, "https://peer.example/other/followers"],
                "published": published_secs_ago(200),
            },
        });
        assert_eq!(
            followers_collection(&addressed),
            "https://peer.example/other/followers",
            "the address the activity itself carries wins"
        );
        let policy = ObjectAgePolicy::new(Some(
            json!({"threshold": 100, "actions": ["strip_followers"]}),
        ));
        let stripped = policy
            .filter(&ctx(), addressed.clone())
            .expect("strip never rejects");
        assert_eq!(
            stripped["to"],
            json!([PUBLIC]),
            "the addressed collection left `to`"
        );
        assert_eq!(
            stripped["object"]["to"],
            json!([PUBLIC]),
            "on both documents"
        );
        assert_eq!(
            policy.filter(&ctx(), addressed),
            Ok(stripped),
            "and the run is golden-stable"
        );
    }
}
