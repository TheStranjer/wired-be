//! The compiled `simple` configuration: [`SimpleConfig`], the eleven
//! [`DomainMatcher`]s of one read of the settings, the answers the chain and
//! the instance endpoints ask of them, and the published form of it all.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use tracing::warn;

use crate::federation::mrf::Transparency;

use super::{
    SIMPLE_INFO_KEY, SIMPLE_KEY,
    lists::{List, Reason},
    matcher::{DomainMatcher, Publication, matcher_for},
};

/// The compiled `simple` configuration: the policy's eleven domain lists,
/// built once per configuration load and shared by every chain that runs
/// them and by [`rejected`](crate::federation::mrf::MrfConfig::rejected).
#[derive(Clone, Debug, Default)]
pub struct SimpleConfig {
    /// Each configured list by name; a list nothing usable was written
    /// into is absent, which reads exactly as an empty one.
    lists: BTreeMap<List, DomainMatcher>,
}

impl SimpleConfig {
    /// Compiles the `simple` key of the `mrf` instance setting. `params` is
    /// that key's value, `None` when the setting carries none; a missing
    /// key, a non-object document and a malformed list all read as the
    /// empty configuration — the policy then governs nothing, and the
    /// warning says what was dropped.
    #[must_use]
    pub fn from_params(params: Option<&Value>) -> Self {
        let Some(document) = params else {
            return Self::default();
        };
        let Value::Object(fields) = document else {
            warn!("the `simple` MRF setting is not a JSON object; the policy governs nothing");
            return Self::default();
        };
        Self::from_fields(fields)
    }

    /// The configuration of one setting object: its recognizable keys
    /// compiled, its unknown ones warned about and skipped.
    fn from_fields(fields: &Map<String, Value>) -> Self {
        let mut lists: BTreeMap<List, DomainMatcher> = BTreeMap::new();
        for (key, value) in fields {
            let Some(list) = List::from_key(key) else {
                warn!(key, "an unknown `mrf.simple` list was configured; ignored");
                continue;
            };
            let _previous = lists.insert(list, matcher_for(list, value));
        }
        Self { lists }
    }

    /// The matcher `list` was configured with, `None` when it holds
    /// nothing usable.
    #[must_use]
    pub fn matcher(&self, list: List) -> Option<&DomainMatcher> {
        self.lists.get(&list).filter(|matcher| !matcher.is_empty())
    }

    /// Whether the operator wrote anything usable into `list`.
    #[must_use]
    pub fn configured(&self, list: List) -> bool {
        self.matcher(list).is_some()
    }

    /// The reason `list` names `host`, `None` when that list does not.
    #[must_use]
    pub fn reason(&self, list: List, host: &str) -> Option<&Reason> {
        self.matcher(list)?.matches(host)
    }

    /// Whether `list` names `host`.
    #[must_use]
    pub fn matches(&self, list: List, host: &str) -> bool {
        self.reason(list, host).is_some()
    }

    /// Whether the domain lists refuse `host` outright: `reject` names it,
    /// or the `accept` allowlist is in force and does not name it.
    ///
    /// This is the answer with no local-domain exemption in it: the chain
    /// adds that one (Pleroma's `check_accept` keeps its own instance
    /// federating), while the inbox and the delivery paths never need it
    /// because they only ever ask about foreign hosts.
    #[must_use]
    pub fn rejected(&self, host: &str) -> bool {
        if self.matches(List::Reject, host) {
            return true;
        }
        self.configured(List::Accept) && !self.matches(List::Accept, host)
    }

    /// The entries of `list` public output may show, in configured order:
    /// pattern verbatim beside reason, the excluded hosts dropped. A list
    /// nothing usable was written into — or everything usable was written
    /// out of, by way of an exclusion — answers the empty vector.
    #[must_use]
    pub fn published(&self, list: List, transparency: &Transparency) -> Vec<Publication<'_>> {
        self.lists
            .get(&list)
            .map_or_else(Vec::new, |matcher| matcher.published(transparency))
    }

    /// The policy's transparency blocks, keyed for the federation
    /// metadata: Pleroma's `mrf_simple`, every list under its own name
    /// with the patterns it holds verbatim, beside `mrf_simple_info` — the
    /// `{pattern: {"reason": …}}` object of each list whose visible
    /// entries carry a reason, absent for a list whose entries carry none.
    ///
    /// Every list is published, not only the ones with something in them:
    /// Pleroma maps its whole `:mrf_simple` keyword list, so an instance
    /// that rejects nothing still says which lists it could fill. A host
    /// the transparency settings exclude appears in neither object.
    #[must_use]
    pub fn describe(&self, transparency: &Transparency) -> Map<String, Value> {
        let mut lists = Map::new();
        let mut info = Map::new();
        for list in List::ALL {
            let published = self.published(list, transparency);
            let _pattern = lists.insert(
                list.key().to_owned(),
                Value::Array(
                    published
                        .iter()
                        .map(|entry| Value::String(entry.pattern.to_owned()))
                        .collect(),
                ),
            );
            let reasons = reason_object(&published);
            if !reasons.is_empty() {
                let _reason = info.insert(list.key().to_owned(), Value::Object(reasons));
            }
        }
        Map::from_iter([
            (SIMPLE_KEY.to_owned(), Value::Object(lists)),
            (SIMPLE_INFO_KEY.to_owned(), Value::Object(info)),
        ])
    }
}

/// The `{pattern: {"reason": …}}` object of the published `entries` that
/// carry one; empty when none does, which is what keeps a reasonless list
/// out of `mrf_simple_info`.
fn reason_object(entries: &[Publication<'_>]) -> Map<String, Value> {
    let mut reasons = Map::new();
    for entry in entries {
        if entry.reason.is_empty() {
            continue;
        }
        let _reason = reasons.insert(entry.pattern.to_owned(), json!({ "reason": entry.reason }));
    }
    reasons
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::federation::mrf::simple::test_support::{LOCAL, PEER};

    /// [`SimpleConfig::from_params`]: the shapes an entry may take, and
    /// what is dropped rather than trusted.
    #[test]
    fn entries_are_read_from_every_configured_spelling() {
        let config = SimpleConfig::from_params(Some(&json!({
            "reject": ["plain.example", ["paired.example", "spam"], ["one-element.example"]],
            "media_nsfw": "not-an-array",
            "nonsense": ["who.example"],
            "accept": [7, null, ["good.example", {"reason": "an object"}]],
        })));
        assert!(
            config.matches(List::Reject, "plain.example"),
            "a bare string is the pattern alone"
        );
        assert_eq!(
            config
                .reason(List::Reject, "paired.example")
                .map(Reason::text),
            Some("spam"),
            "the pair carries its reason"
        );
        assert!(
            config.matches(List::Reject, "one-element.example"),
            "a one-element pair is a pattern alone"
        );
        assert!(
            !config.configured(List::MediaNsfw),
            "a list that is not an array is warned about and empty"
        );
        assert!(
            !config.configured(List::Accept),
            "every unusable entry of a list drops out, and nothing usable was in this one"
        );
    }

    /// [`SimpleConfig::from_params`]: no parameters, a non-object document
    /// and an unknown key all leave the policy with nothing to enforce.
    #[test]
    fn a_configuration_that_names_nothing_governs_nothing() {
        for params in [None, Some(json!("nope")), Some(json!({}))] {
            let config = SimpleConfig::from_params(params.as_ref());
            assert!(
                !config.rejected("anyone.example"),
                "{params:?} configures no list at all"
            );
            assert_eq!(
                config.describe(&Transparency::open())["mrf_simple"]["reject"],
                json!([]),
                "and it publishes an empty list wherever a rule could go"
            );
        }
    }

    /// The transparency exclusions reach both published objects: an
    /// excluded host is simply not there, and a list left empty by
    /// excluding all of it stays a published, empty key.
    #[test]
    fn the_excluded_hosts_are_missing_from_every_published_object() {
        let lists = json!({
            "reject": [["evil.example", "bots"], "*.partner.example", "press.example"],
            "media_nsfw": ["press.example"],
            "followers_only": ["quiet.example"],
        });
        let config = SimpleConfig::from_params(Some(&lists));
        let transparency =
            Transparency::hiding(&["*.partner.example".to_owned(), "press.example".to_owned()]);

        let described = config.describe(&transparency);
        assert_eq!(
            described["mrf_simple"]["reject"],
            json!(["evil.example"]),
            "the two excluded patterns are gone, the third stands"
        );
        assert_eq!(
            described["mrf_simple"]["media_nsfw"],
            json!([]),
            "a list emptied of its only entry is still a published list"
        );
        assert_eq!(
            described["mrf_simple"]["followers_only"],
            json!(["quiet.example"]),
            "and an untouched list keeps what it had"
        );
        assert_eq!(
            described["mrf_simple_info"],
            json!({"reject": {"evil.example": {"reason": "bots"}}}),
            "an excluded host's reason is gone with it"
        );
        assert!(
            !transparency.hides("evil.example"),
            "and the exclusion says nothing about the host it does not name"
        );
    }

    /// [`SimpleConfig::published`] is the same read the two objects are
    /// built from: pattern verbatim, reason as recorded, nothing excluded.
    #[test]
    fn publishing_lists_keeps_patterns_and_reasons_together() {
        let config = SimpleConfig::from_params(Some(&json!({
            "reject": [["evil.example", "bots"], "*.worse.example"],
            "avatar_removal": [],
        })));
        let open = Transparency::open();
        assert_eq!(
            config.published(List::Reject, &open),
            vec![
                Publication {
                    pattern: "evil.example",
                    reason: "bots",
                },
                Publication {
                    pattern: "*.worse.example",
                    reason: "",
                },
            ],
            "the entries in configured order, each with its reason"
        );
        assert!(
            config.published(List::AvatarRemoval, &open).is_empty()
                && config.published(List::Accept, &open).is_empty(),
            "a list nothing usable was written into publishes nothing"
        );
        assert!(
            config
                .published(
                    List::Reject,
                    &Transparency::hiding(&["*.example".to_owned()])
                )
                .is_empty(),
            "and one exclusion covers the whole list here: both entries sit \
             under `*.example`"
        );
    }

    /// [`SimpleConfig::rejected`] is the allowlist and the reject list as
    /// one fact, with no local exemption in it.
    #[test]
    fn the_compiled_lists_answer_who_is_rejected() {
        let config = SimpleConfig::from_params(Some(&json!({"reject": ["*.evil.example"]})));
        assert!(
            config.rejected("cdn.evil.example"),
            "a host below the reject entry is rejected"
        );
        assert!(!config.rejected(PEER), "a host no list names is not");

        let allowlist = SimpleConfig::from_params(Some(&json!({
            "accept": [["friend.example", "our only peer"]],
            "reject": ["evil.example"],
        })));
        assert!(
            !allowlist.rejected("friend.example"),
            "the allowlisted peer"
        );
        assert!(
            allowlist.rejected(LOCAL),
            "everything else — this instance's own domain included: the chain adds \
             that exemption, the raw answer does not"
        );
        assert!(
            allowlist.rejected("evil.example"),
            "and a rejected host stays rejected"
        );
    }
}
