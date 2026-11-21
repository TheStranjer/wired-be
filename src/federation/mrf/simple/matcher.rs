//! What a configured list becomes once the parameters are read: [`Rule`]s,
//! one per compiled [`Entry`], held by the [`DomainMatcher`] that answers
//! whether a list names a host. [`Publication`] is one rule as public
//! output shows it, and [`matcher_for`] compiles one configured list value.

use serde_json::Value;
use tracing::warn;

use crate::federation::mrf::Transparency;

use super::lists::{Entry, List, Reason};

/// One list entry as public output shows it: the pattern exactly as
/// configured — a `*.example.com` wildcard is masked by nothing — beside
/// the reason the operator recorded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Publication<'a> {
    /// The pattern as configured, lowercased and trimmed.
    pub pattern: &'a str,
    /// The reason recorded beside it; empty when the operator gave none.
    pub reason: &'a str,
}

/// One compiled list entry: the pattern split into what it matches, beside
/// the reason it carries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Rule {
    /// The pattern as configured, lowercased and trimmed: what an exact
    /// entry compares against, and what transparency reports.
    pub(super) pattern: String,
    /// The domain the rule stands for — the pattern itself, or the part a
    /// `*.` wildcard covers.
    pub(super) stem: String,
    /// Whether the pattern is the `*.domain` wildcard form.
    pub(super) wildcard: bool,
    /// Why the entry is there.
    pub(super) reason: Reason,
}

impl Rule {
    /// Whether `host` — already lowercased by [`DomainMatcher::matches`] —
    /// is this rule's host: the exact pattern, or, under a wildcard, that
    /// domain itself or any domain strictly below it. A look-alike tail
    /// (`evil-example.com` under `*.example.com`) is not below anything:
    /// the boundary has to be a dot.
    fn matches(&self, host: &str) -> bool {
        if !self.wildcard {
            return host == self.pattern;
        }
        host == self.stem || is_below(host, &self.stem)
    }
}

/// Whether `host` sits strictly below `stem` (both already lowercase).
fn is_below(host: &str, stem: &str) -> bool {
    host.strip_suffix(stem)
        .is_some_and(|above| above.ends_with('.'))
}

/// One configured domain list, precompiled for matching.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DomainMatcher {
    /// The compiled rules, in configured order: the first that matches
    /// answers.
    rules: Vec<Rule>,
}

impl DomainMatcher {
    /// Compiles `entries` into the matcher's rules. Blank and malformed
    /// patterns drop out (their warning says which), so a list nothing
    /// usable was written into matches nobody at all.
    #[must_use]
    pub fn new(entries: &[Entry]) -> Self {
        Self {
            rules: entries.iter().filter_map(Entry::rule).collect(),
        }
    }

    /// The reason of the first rule naming `host`, `None` when the list
    /// holds nothing for it. Hosts compare case-insensitively, and a
    /// `*.example.com` entry covers `example.com` and everything below it.
    #[must_use]
    pub fn matches(&self, host: &str) -> Option<&Reason> {
        let host = host.to_ascii_lowercase();
        self.rules
            .iter()
            .find(|rule| rule.matches(&host))
            .map(|rule| &rule.reason)
    }

    /// Whether the list names `host` at all.
    #[must_use]
    pub fn contains(&self, host: &str) -> bool {
        self.matches(host).is_some()
    }

    /// Whether nothing usable was configured for this list.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The matcher of a plain pattern list — no reasons: what the
    /// transparency exclusions compile to, matched with the same rules the
    /// policy lists themselves are matched with so that excluding
    /// `*.example.com` hides that entry and every entry below it.
    #[must_use]
    pub fn from_patterns(patterns: &[String]) -> Self {
        let entries = patterns
            .iter()
            .map(|pattern| Entry::new(pattern, None))
            .collect::<Vec<Entry>>();
        Self::new(&entries)
    }

    /// The entries public output may show: every rule's pattern verbatim
    /// beside its reason, in configured order, with the hosts the
    /// transparency settings exclude dropped.
    pub(super) fn published(&self, transparency: &Transparency) -> Vec<Publication<'_>> {
        self.rules
            .iter()
            .filter(|rule| !transparency.hides(&rule.pattern))
            .map(|rule| Publication {
                pattern: &rule.pattern,
                reason: rule.reason.text(),
            })
            .collect()
    }
}

/// The matcher of one configured list value. A value that is not an array
/// is warned about and reads as the empty list.
pub(super) fn matcher_for(list: List, value: &Value) -> DomainMatcher {
    let Value::Array(entries) = value else {
        warn!(
            key = list.key(),
            "an `mrf.simple` list is not an array; the list stays empty"
        );
        return DomainMatcher::default();
    };
    let parsed = entries
        .iter()
        .filter_map(Entry::from_value)
        .collect::<Vec<Entry>>();
    DomainMatcher::new(&parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`DomainMatcher`]: an exact entry names its host and nobody else,
    /// whatever case either side arrives in.
    #[test]
    fn an_exact_entry_matches_its_host_in_any_case() {
        let matcher = DomainMatcher::new(&[Entry::new("Peer.Example", None)]);
        for host in ["peer.example", "PEER.EXAMPLE", "Peer.example"] {
            assert!(matcher.contains(host), "`{host}` is the entry's host");
        }
        for host in [
            "evil.example",
            "sub.peer.example",
            "peer.example.evil.net",
            "",
        ] {
            assert!(!matcher.contains(host), "`{host}` is not the entry's host");
        }
    }

    /// [`DomainMatcher`]: a `*.` entry covers its own domain and every
    /// domain below it, at any depth, and no look-alike tail.
    #[test]
    fn a_wildcard_entry_covers_every_domain_below_it() {
        let matcher = DomainMatcher::new(&[Entry::new("*.peer.example", None)]);
        for host in [
            "peer.example",
            "api.peer.example",
            "a.b.c.peer.example",
            "CDN.Peer.Example",
        ] {
            assert!(matcher.contains(host), "`{host}` is at or below the entry");
        }
        for host in [
            "evil.example",
            "notpeer.example",
            "evil-peer.example",
            "example",
        ] {
            assert!(
                !matcher.contains(host),
                "`{host}` is nowhere below the entry"
            );
        }
    }

    /// [`DomainMatcher`]: the first entry that matches answers, with the
    /// reason it was configured with, and nothing matches an empty list.
    #[test]
    fn the_first_matching_entry_answers_with_its_reason() {
        let matcher = DomainMatcher::new(&[
            Entry::new("*.example", Some("broad")),
            Entry::new("peer.example", Some("exact")),
        ]);
        assert_eq!(
            matcher.matches("peer.example").map(Reason::text),
            Some("broad"),
            "the earlier entry wins, as in Pleroma's `Enum.any?`"
        );
        assert_eq!(
            matcher.matches("other.example").map(Reason::text),
            Some("broad"),
            "and it still covers what is below it"
        );
        assert_eq!(
            DomainMatcher::new(&[Entry::new("peer.example", None)])
                .matches("peer.example")
                .map(Reason::text),
            Some(""),
            "an entry without a reason carries the empty one"
        );
        assert!(
            DomainMatcher::new(&[]).matches("peer.example").is_none(),
            "nothing was configured, so nothing matches"
        );
    }
}
