//! Pleroma's `HashtagPolicy`: the hashtag switches of the
//! [pipeline](super).
//!
//! The reference is `lib/pleroma/web/activity_pub/mrf/hashtag_policy.ex`.
//! Pleroma appends it to every chain (`lib/pleroma/web/activity_pub/mrf.ex`)
//! with the defaults `sensitive: ["nsfw"], reject: [],
//! federated_timeline_removal: []`, so a post from a Misskey or Pleroma
//! user tagged `#nsfw` without a content warning arrives hidden. The three
//! lists live under the `hashtag` key of the `mrf` instance setting
//! (`:mrf_hashtag` in the admin config store), as lowercase tag names
//! without the `#`:
//!
//! ```json
//! {"hashtag": {"sensitive": ["nsfw", "lewd"],
//!              "reject": ["spam"],
//!              "federated_timeline_removal": ["politics"]}}
//! ```
//!
//! The policy speaks about `Create`/`Update` notes of remote actors only,
//! and compares the names of the note's `Hashtag` tags, `#`-stripped and
//! lowercased:
//!
//! * `reject`: any match refuses the activity.
//! * `federated_timeline_removal`: any match moves the public address from
//!   `to` into `cc`, on the activity and its note — the note arrives
//!   unlisted.
//! * `sensitive`: any match sets the note's `sensitive` flag.
//!
//! A list the setting leaves out keeps its default; a list that is not an
//! array of strings is warned about and keeps its default too. Entries are
//! normalized as the tag names are, so `"#NSFW"` and `"nsfw"` configure the
//! same tag. The policy is not named by `mrf.policies`: [`build_chain`]
//! appends it after the configured ones, always.
//!
//! [`build_chain`]: super::build_chain

use serde_json::{Map, Value, json};
use tracing::warn;

use crate::federation::{
    json,
    mrf::{
        MrfContext, MrfPolicy, MrfResult, Transparency,
        simple::{
            activity_host, is_note_activity, note_object_mut, rewrite_each_document,
            unlist_document,
        },
        single_block,
    },
};

/// The key of this policy's parameters inside the `mrf` instance setting.
pub const PARAMS_KEY: &str = "hashtag";
/// The `ActivityStreams` type of a hashtag entry in a note's `tag`.
const HASHTAG_TYPE: &str = "Hashtag";
/// The tag Pleroma marks sensitive when nothing is configured.
const DEFAULT_SENSITIVE: &str = "nsfw";
/// The list key that marks matching notes sensitive.
const SENSITIVE_KEY: &str = "sensitive";
/// The list key that refuses matching notes.
const REJECT_KEY: &str = "reject";
/// The list key that unlists matching notes.
const FEDERATED_TIMELINE_REMOVAL_KEY: &str = "federated_timeline_removal";

/// One tag name in the form both the lists and the notes are compared in:
/// no leading `#`, lowercase.
fn normalize(name: &str) -> String {
    name.trim().trim_start_matches('#').to_lowercase()
}

/// The names of the `Hashtag` entries of a note's `tag`, normalized.
fn note_hashtags(activity: &Value) -> Vec<String> {
    let Some(tags) = activity.get("object").and_then(|object| object.get("tag")) else {
        return Vec::new();
    };
    json::as_array(tags)
        .iter()
        .filter(|entry| {
            entry
                .get("type")
                .and_then(json::first_type)
                .is_some_and(|kind| kind == HASHTAG_TYPE)
        })
        .filter_map(|entry| entry.get("name").and_then(Value::as_str))
        .map(normalize)
        .filter(|name| !name.is_empty())
        .collect()
}

/// One configured list: the normalized entries of the array under `key`,
/// `default` when the key is absent, and `default` (beside the warning that
/// says why) when it holds anything but an array of strings.
fn list(fields: Option<&Map<String, Value>>, key: &str, default: &[&str]) -> Vec<String> {
    let fallback = || -> Vec<String> { default.iter().map(|name| (*name).to_owned()).collect() };
    let Some(value) = fields.and_then(|fields| fields.get(key)) else {
        return fallback();
    };
    let names: Option<Vec<String>> = value.as_array().and_then(|entries| {
        entries
            .iter()
            .map(|entry| entry.as_str().map(normalize))
            .collect()
    });
    names.unwrap_or_else(|| {
        warn!(
            key,
            "an `mrf.hashtag` list is not an array of strings; its default applies"
        );
        fallback()
    })
}

/// The hashtag switches: Pleroma's `MRF.HashtagPolicy`.
#[derive(Clone, Debug)]
pub struct HashtagPolicy {
    /// The tags that mark a note sensitive.
    sensitive: Vec<String>,
    /// The tags that refuse a note.
    reject: Vec<String>,
    /// The tags that unlist a note.
    federated_timeline_removal: Vec<String>,
}

impl HashtagPolicy {
    /// The policy over the `hashtag` key of the `mrf` setting (`None` when
    /// the setting carries none): each list as configured, defaulting when
    /// its key is absent or unusable.
    #[must_use]
    pub fn new(params: Option<&Value>) -> Self {
        let fields = params.and_then(Value::as_object);
        if params.is_some() && fields.is_none() {
            warn!(
                "the `hashtag` MRF setting is not a JSON object; \
                 the policy runs on its default lists"
            );
        }
        Self {
            sensitive: list(fields, SENSITIVE_KEY, &[DEFAULT_SENSITIVE]),
            reject: list(fields, REJECT_KEY, &[]),
            federated_timeline_removal: list(fields, FEDERATED_TIMELINE_REMOVAL_KEY, &[]),
        }
    }

    /// Whether any of the note's `tags` is on `configured`.
    fn matches(configured: &[String], tags: &[String]) -> bool {
        tags.iter().any(|tag| configured.contains(tag))
    }
}

/// Whether the activity is a note `Create`/`Update` by an actor of a
/// foreign domain; a host the policy cannot read is not a foreign one.
fn is_remote_note(ctx: &MrfContext, activity: &Value) -> bool {
    is_note_activity(activity)
        && activity_host(activity).is_some_and(|host| !host.eq_ignore_ascii_case(&ctx.local_domain))
}

impl MrfPolicy for HashtagPolicy {
    fn name(&self) -> &'static str {
        "HashtagPolicy"
    }

    fn filter(&self, ctx: &MrfContext, activity: Value) -> MrfResult {
        if !is_remote_note(ctx, &activity) {
            return Ok(activity);
        }
        let tags = note_hashtags(&activity);
        if Self::matches(&self.reject, &tags) {
            return Err("[HashtagPolicy] the note carries a rejected hashtag".to_owned());
        }
        let mut activity = activity;
        if Self::matches(&self.federated_timeline_removal, &tags) {
            rewrite_each_document(&mut activity, unlist_document);
        }
        if Self::matches(&self.sensitive, &tags)
            && let Some(object) = note_object_mut(&mut activity)
        {
            let _previous = object.insert("sensitive".to_owned(), Value::Bool(true));
        }
        Ok(activity)
    }

    fn describe(&self, _transparency: &Transparency) -> Map<String, Value> {
        single_block(
            self.name(),
            json!({
                "sensitive": self.sensitive,
                "reject": self.reject,
                "federated_timeline_removal": self.federated_timeline_removal,
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;

    use super::*;
    use crate::federation::mrf::{MrfConfig, simple::PUBLIC_ADDRESS};

    /// The remote author the fixtures post as.
    const ALICE: &str = "https://peer.example/users/alice";
    /// The note a fixture `Create` announces.
    const NOTE: &str = "https://peer.example/objects/1";

    /// The context of a run: only the local domain is read.
    fn ctx() -> MrfContext {
        MrfContext {
            config: Arc::new(MrfConfig::assemble(Vec::new(), Map::new())),
            now: Utc::now(),
            local_domain: "test.example".to_owned(),
            author_follower_count: None,
            author_status_count: None,
            author_tags: Vec::new(),
            target_tags: Vec::new(),
        }
    }

    /// A public `Create` of a note by `actor` carrying `tags` as `Hashtag`
    /// entries, beside a `Mention` that is no hashtag.
    fn create_by(actor: &str, tags: &[&str]) -> Value {
        let mut entries: Vec<Value> = tags
            .iter()
            .map(|name| json!({"type": "Hashtag", "name": name, "href": "https://peer.example/tags/x"}))
            .collect();
        entries
            .push(json!({"type": "Mention", "name": "#nsfw", "href": "https://x.example/u/bob"}));
        json!({
            "id": "https://peer.example/activities/1",
            "type": "Create",
            "actor": actor,
            "to": [PUBLIC_ADDRESS],
            "cc": [],
            "object": {
                "id": NOTE,
                "type": "Note",
                "attributedTo": actor,
                "to": [PUBLIC_ADDRESS],
                "cc": [],
                "content": "<p>hello</p>",
                "tag": entries,
            },
        })
    }

    /// [`create_by`] the remote author.
    fn create(tags: &[&str]) -> Value {
        create_by(ALICE, tags)
    }

    /// The golden of [`create`] after the sensitive switch.
    fn marked(tags: &[&str]) -> Value {
        let mut expected = create(tags);
        expected["object"]["sensitive"] = json!(true);
        expected
    }

    /// The golden of [`create`] after the timeline removal.
    fn unlisted(tags: &[&str]) -> Value {
        let mut expected = create(tags);
        expected["to"] = json!([]);
        expected["cc"] = json!([PUBLIC_ADDRESS]);
        expected["object"]["to"] = json!([]);
        expected["object"]["cc"] = json!([PUBLIC_ADDRESS]);
        expected
    }

    /// With no configuration the policy still marks `#nsfw` notes
    /// sensitive, and a note without the tag stands untouched.
    #[test]
    fn the_default_marks_nsfw_notes_sensitive() {
        let policy = HashtagPolicy::new(None);

        assert_eq!(
            policy.filter(&ctx(), create(&["nsfw"])),
            Ok(marked(&["nsfw"])),
            "the default list holds nsfw"
        );
        assert_eq!(
            policy.filter(&ctx(), create(&["cats"])),
            Ok(create(&["cats"])),
            "other tags are not switched"
        );
        assert_eq!(
            policy.filter(&ctx(), create(&[])),
            Ok(create(&[])),
            "a note without tags is not switched"
        );
    }

    /// Tag names compare without the `#` and without case, on both sides:
    /// the note's and the configured list's.
    #[test]
    fn matching_ignores_case_and_the_hash_sign() {
        let policy = HashtagPolicy::new(None);
        assert_eq!(
            policy.filter(&ctx(), create(&["NSFW"])),
            Ok(marked(&["NSFW"])),
            "an upper-case tag matches"
        );
        assert_eq!(
            policy.filter(&ctx(), create(&["#NsFw"])),
            Ok(marked(&["#NsFw"])),
            "a hash-prefixed tag matches"
        );
        let configured = HashtagPolicy::new(Some(&json!({"sensitive": ["#Lewd"]})));
        assert_eq!(
            configured.filter(&ctx(), create(&["lewd"])),
            Ok(marked(&["lewd"])),
            "a configured entry is normalized too"
        );
        assert_eq!(
            configured.filter(&ctx(), create(&["nsfw"])),
            Ok(create(&["nsfw"])),
            "a configured list replaces the default"
        );
    }

    /// A `reject` match refuses the activity, and wins over the other
    /// switches.
    #[test]
    fn the_reject_list_refuses_the_note() {
        let policy = HashtagPolicy::new(Some(&json!({"reject": ["spam"]})));
        assert_eq!(
            policy.filter(&ctx(), create(&["Spam", "nsfw"])),
            Err("[HashtagPolicy] the note carries a rejected hashtag".to_owned()),
            "the rejected tag ends the chain"
        );
        assert_eq!(
            policy.filter(&ctx(), create(&["cats"])),
            Ok(create(&["cats"])),
            "a note without it passes"
        );
    }

    /// A `federated_timeline_removal` match unlists the note, and combines
    /// with the sensitive switch.
    #[test]
    fn the_timeline_removal_list_unlists_the_note() {
        let policy = HashtagPolicy::new(Some(&json!({"federated_timeline_removal": ["politics"]})));
        assert_eq!(
            policy.filter(&ctx(), create(&["politics"])),
            Ok(unlisted(&["politics"])),
            "the public address moves into cc on both documents"
        );
        let mut both = unlisted(&["politics", "nsfw"]);
        both["object"]["sensitive"] = json!(true);
        assert_eq!(
            policy.filter(&ctx(), create(&["politics", "nsfw"])),
            Ok(both),
            "the default sensitive list still applies beside it"
        );
    }

    /// A local actor's note is never touched, whatever it is tagged.
    #[test]
    fn a_local_actor_is_untouched() {
        let policy = HashtagPolicy::new(Some(&json!({"reject": ["nsfw"]})));
        let local = create_by("https://TEST.example/users/bob", &["nsfw"]);
        assert_eq!(
            policy.filter(&ctx(), local.clone()),
            Ok(local),
            "the local domain is compared without case"
        );
    }

    /// Anything but a note `Create`/`Update` passes: a `Delete`, an
    /// `Announce`, and a `Create` of a non-note object.
    #[test]
    fn non_note_activities_are_untouched() {
        let policy = HashtagPolicy::new(Some(&json!({"reject": ["nsfw"]})));
        let mut delete = create(&["nsfw"]);
        delete["type"] = json!("Delete");
        let mut announce = create(&["nsfw"]);
        announce["type"] = json!("Announce");
        let mut profile = create(&["nsfw"]);
        profile["object"]["type"] = json!("Profile");
        for activity in [delete, announce, profile] {
            assert_eq!(
                policy.filter(&ctx(), activity.clone()),
                Ok(activity),
                "only note Creates and Updates are judged"
            );
        }
        let mut update = create(&["nsfw"]);
        update["type"] = json!("Update");
        assert!(
            policy.filter(&ctx(), update).is_err(),
            "an Update is judged like a Create"
        );
    }

    /// A single `tag` object (not an array) is read as the one-entry list
    /// it stands for.
    #[test]
    fn a_single_tag_object_is_read() {
        let mut activity = create(&[]);
        activity["object"]["tag"] = json!({"type": "Hashtag", "name": "#nsfw"});
        let mut expected = activity.clone();
        expected["object"]["sensitive"] = json!(true);
        assert_eq!(
            HashtagPolicy::new(None).filter(&ctx(), activity),
            Ok(expected),
            "the lone entry counts"
        );
    }

    /// Unusable lists fall back to their defaults.
    #[test]
    fn unusable_lists_keep_their_defaults() {
        for params in [
            json!("nope"),
            json!({"sensitive": "nsfw"}),
            json!({"sensitive": [1, 2]}),
        ] {
            let policy = HashtagPolicy::new(Some(&params));
            assert_eq!(
                policy.sensitive,
                vec!["nsfw".to_owned()],
                "{params} leaves the sensitive default"
            );
            assert!(policy.reject.is_empty(), "{params} leaves reject empty");
        }
    }

    /// The policy names itself and publishes its effective lists under
    /// `mrf_hashtag`, defaults included, as Pleroma's `describe/0` does.
    #[test]
    fn the_policy_names_itself_and_describes_its_lists() {
        let policy = HashtagPolicy::new(Some(&json!({"reject": ["Spam"]})));
        assert_eq!(policy.name(), "HashtagPolicy", "Pleroma's module name");
        assert_eq!(
            Value::Object(policy.describe(&Transparency::open())),
            json!({"mrf_hashtag": {
                "sensitive": ["nsfw"],
                "reject": ["spam"],
                "federated_timeline_removal": [],
            }}),
            "every list, normalized, under the policy's own key"
        );
    }
}
