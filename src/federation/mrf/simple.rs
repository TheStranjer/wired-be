//! Pleroma's `SimplePolicy`: the domain lists of the [pipeline](super).
//!
//! The reference is `lib/pleroma/web/activity_pub/mrf/simple_policy.ex` —
//! "filter activities depending on their origin instance": eleven named
//! lists of instance domains under the `simple` key of the `mrf` instance
//! setting. Every entry is either an exact host or a `*.host` wildcard
//! (both compared case-insensitively), and may carry the operator's reason
//! beside it as a `[pattern, reason]` pair:
//!
//! ```json
//! {"policies": ["simple"],
//!  "simple": {"reject": [["evil.example", "bots"]],
//!             "federated_timeline_removal": ["*.noisy.example"]}}
//! ```
//!
//! The host every rule compares is the host of the activity's `actor`:
//!
//! * [`List::Accept`] is the allowlist: once it holds anything, no other
//!   host federates with this instance at all — this instance's own domain
//!   is never excluded, exactly as Pleroma's `check_accept` keeps it out.
//!   [`List::Reject`] refuses the hosts it names, everything they send
//!   included: Pleroma's `reject` arm lets deletions through, but here the
//!   same [`rejected`](super::MrfConfig::rejected) answer governs the inbox
//!   door and the delivery path, and that one cannot make an exception the
//!   queue would then undo. [`List::RejectDeletes`] is the list for a host
//!   whose stored content may still be erased while nothing else from it
//!   is admitted.
//! * [`List::FederatedTimelineRemoval`] moves the public address of a
//!   `Create`/`Update` note from `to` into `cc`, which
//!   [`parse_note`](crate::federation::note_parser::parse_note) reads back
//!   as `unlisted`; [`List::FollowersOnly`] takes the public address out of
//!   both and puts the author's followers collection in `to`, which reads
//!   back as `private`.
//! * [`List::MediaRemoval`] drops the `attachment` entries of those notes;
//!   [`List::MediaNsfw`] marks the ones that had any `sensitive`.
//! * [`List::ReportRemoval`] refuses `Flag` activities.
//! * [`List::AvatarRemoval`], [`List::BannerRemoval`] and
//!   [`List::BackgroundRemoval`] strip `icon`, `image` and `background`
//!   from the actor documents of their hosts. An actor document is not an
//!   activity, so that half of the policy runs through
//!   [`filter_actor`](super::filter_actor) instead of
//!   [`filter`](super::MrfPolicy::filter).
//!
//! The lists are compiled once, when the configuration is read:
//! [`DomainMatcher`] holds the rules of one list, [`SimpleConfig`] the
//! eleven of one configuration, so a busy inbox matches instead of
//! reparsing JSON, and [`rejected`](super::MrfConfig::rejected) answers
//! from the very same rules.
//!
//! The same rules are what the instance documents publish:
//! [`SimpleConfig::describe`] answers Pleroma's `mrf_simple` and
//! `mrf_simple_info` objects for the federation metadata, and
//! [`SimpleConfig::published`] is the per-list read
//! [`crate::api::instance`] renders as Mastodon domain blocks. Both go
//! through the operator's [`Transparency`], which keeps the hosts of
//! `MRF_TRANSPARENCY_EXCLUSIONS` out of public output while they stay on
//! the list and in force.

use std::{collections::BTreeMap, fmt, sync::Arc};

use serde_json::{Map, Value, json};
use tracing::warn;

use crate::federation::{
    json,
    mrf::{MrfContext, MrfPolicy, MrfResult, Transparency},
    note_parser::{OBJECT_TYPES, PUBLIC_RECIPIENTS},
};

/// The key of this policy's parameters inside the `mrf` instance setting,
/// the name [`build_chain`](super::build_chain) resolves, and the list
/// [`rejected`](super::MrfConfig::rejected) reads its allowlist and its
/// reject list from.
pub const PARAMS_KEY: &str = "simple";
/// The prefix of a wildcard pattern: `*.example.com` stands for
/// `example.com` and for every host below it (Pleroma's
/// `subdomains_regex`).
const WILDCARD_PREFIX: &str = "*.";
/// The activity types whose note the content rules act on.
const WRAPPING_TYPES: [&str; 2] = ["Create", "Update"];
/// The activity type [`List::RejectDeletes`] refuses.
const DELETE_TYPE: &str = "Delete";
/// The activity type [`List::ReportRemoval`] refuses.
const FLAG_TYPE: &str = "Flag";
/// The public address as this policy writes it back: the full
/// `ActivityStreams` iri, the spelling every reader accepts
/// ([`PUBLIC_RECIPIENTS`] covers the two shorthands it may arrive as).
pub(super) const PUBLIC_ADDRESS: &str = "https://www.w3.org/ns/activitystreams#Public";
/// The addressing properties the note rules rewrite.
pub(super) const ADDRESS_FIELDS: [&str; 2] = ["to", "cc"];
/// The actor-document field each removal list strips, beside its list.
const ACTOR_FIELDS: [(List, &str); 3] = [
    (List::AvatarRemoval, "icon"),
    (List::BannerRemoval, "image"),
    (List::BackgroundRemoval, "background"),
];
/// The suffix of the followers-collection URL this policy falls back to
/// (the convention Pleroma, Mastodon and [`super::super::actor`] all use).
pub(super) const FOLLOWERS_SUFFIX: &str = "/followers";
/// The federation-metadata key this policy's list object is published
/// under: Pleroma's `mrf_simple`.
const SIMPLE_KEY: &str = "mrf_simple";
/// The federation-metadata key of the per-entry reason object beside it:
/// Pleroma's separate `mrf_simple_info`, kept separate for the same
/// backwards-compatibility reason it states — the lists themselves shipped
/// without reasons first.
const SIMPLE_INFO_KEY: &str = "mrf_simple_info";

/// One of the policy's domain lists, named by its configuration key.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum List {
    /// `accept`: the allowlist — once it holds anything, only its hosts
    /// federate with this instance.
    Accept,
    /// `reject`: refuse everything a matching host sends.
    Reject,
    /// `reject_deletes`: refuse `Delete` activities from matching hosts.
    RejectDeletes,
    /// `federated_timeline_removal`: make matching hosts' notes unlisted.
    FederatedTimelineRemoval,
    /// `followers_only`: make matching hosts' public notes followers-only.
    FollowersOnly,
    /// `media_removal`: strip the attachments of matching hosts' notes.
    MediaRemoval,
    /// `media_nsfw`: mark matching hosts' notes that have attachments
    /// sensitive.
    MediaNsfw,
    /// `report_removal`: refuse `Flag` activities from matching hosts.
    ReportRemoval,
    /// `avatar_removal`: drop `icon` from matching hosts' actor documents.
    AvatarRemoval,
    /// `banner_removal`: drop `image` from matching hosts' actor documents.
    BannerRemoval,
    /// `background_removal`: drop `background` from matching hosts' actor
    /// documents.
    BackgroundRemoval,
}

impl List {
    /// Every list, in the order Pleroma documents the configuration.
    pub const ALL: [Self; 11] = [
        Self::Accept,
        Self::Reject,
        Self::RejectDeletes,
        Self::FederatedTimelineRemoval,
        Self::FollowersOnly,
        Self::MediaRemoval,
        Self::MediaNsfw,
        Self::ReportRemoval,
        Self::AvatarRemoval,
        Self::BannerRemoval,
        Self::BackgroundRemoval,
    ];

    /// The key the list is configured under.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Reject => "reject",
            Self::RejectDeletes => "reject_deletes",
            Self::FederatedTimelineRemoval => "federated_timeline_removal",
            Self::FollowersOnly => "followers_only",
            Self::MediaRemoval => "media_removal",
            Self::MediaNsfw => "media_nsfw",
            Self::ReportRemoval => "report_removal",
            Self::AvatarRemoval => "avatar_removal",
            Self::BannerRemoval => "banner_removal",
            Self::BackgroundRemoval => "background_removal",
        }
    }

    /// The list a configuration key names, `None` for any other key.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|list| list.key() == key)
    }
}

/// The reason an operator recorded beside one domain pattern — Pleroma's
/// tuple value. The empty reason says none was given.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Reason(String);

impl Reason {
    /// The reason as written.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.0
    }

    /// Whether no reason was recorded.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The ` (reason)` tail a rejection message carries; empty when no
    /// reason was given, which keeps Pleroma's own wording.
    fn suffix(&self) -> String {
        if self.is_empty() {
            String::new()
        } else {
            format!(" ({})", self.0)
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One configured list entry: a domain pattern beside its reason.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    /// The pattern as configured: an exact host or a `*.host` wildcard.
    pub pattern: String,
    /// Why the entry is there, when the operator said.
    pub reason: Reason,
}

impl Entry {
    /// The entry for `pattern`, with `reason` when one was given.
    #[must_use]
    pub fn new(pattern: &str, reason: Option<&str>) -> Self {
        Self {
            pattern: pattern.to_owned(),
            reason: Reason(reason.unwrap_or_default().to_owned()),
        }
    }

    /// The entry a configured JSON value spells: a bare pattern string, or
    /// the `[pattern]` / `[pattern, reason]` pair Pleroma's config tuples
    /// arrive as. Anything else is warned about and reads as no entry, so
    /// one malformed line cannot void a whole list.
    #[must_use]
    pub fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::String(pattern) => Some(Self::new(pattern, None)),
            Value::Array(parts) => Self::from_parts(parts),
            other @ (Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_)) => {
                warn!(
                    entry = %other,
                    "an `mrf.simple` entry is neither a domain nor a [domain, reason] pair; ignored"
                );
                None
            }
        }
    }

    /// The pair form: the pattern, then the reason when the array carries
    /// a second element. A non-string half or a longer pair is no entry.
    fn from_parts(parts: &[Value]) -> Option<Self> {
        let pattern = parts.first().and_then(Value::as_str)?;
        let reason = match parts.get(1) {
            None => None,
            Some(Value::String(text)) => Some(text.as_str()),
            Some(other) => {
                warn!(
                    entry = %other,
                    "the reason of an `mrf.simple` pair is not a string; the entry is ignored"
                );
                return None;
            }
        };
        if parts.len() > 2 {
            warn!(
                "an `mrf.simple` pair carries more than a domain and a reason; the entry is ignored"
            );
            return None;
        }
        Some(Self::new(pattern, reason))
    }

    /// The compiled rule this entry becomes, `None` (beside the warning
    /// that says why) for a pattern that names no host.
    fn rule(&self) -> Option<Rule> {
        let pattern = self.pattern.trim().to_ascii_lowercase();
        if pattern.is_empty() {
            warn!("an `mrf.simple` entry names no domain; ignored");
            return None;
        }
        let wildcard = pattern.starts_with(WILDCARD_PREFIX);
        let stem = pattern
            .strip_prefix(WILDCARD_PREFIX)
            .unwrap_or(&pattern)
            .to_owned();
        if stem.is_empty() {
            warn!(
                pattern = %self.pattern,
                "an `mrf.simple` wildcard names no domain below it; ignored"
            );
            return None;
        }
        Some(Rule {
            wildcard,
            pattern,
            stem,
            reason: self.reason.clone(),
        })
    }
}

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
struct Rule {
    /// The pattern as configured, lowercased and trimmed: what an exact
    /// entry compares against, and what transparency reports.
    pattern: String,
    /// The domain the rule stands for — the pattern itself, or the part a
    /// `*.` wildcard covers.
    stem: String,
    /// Whether the pattern is the `*.domain` wildcard form.
    wildcard: bool,
    /// Why the entry is there.
    reason: Reason,
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
    fn published(&self, transparency: &Transparency) -> Vec<Publication<'_>> {
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

/// The compiled `simple` configuration: the policy's eleven domain lists,
/// built once per configuration load and shared by every chain that runs
/// them and by [`rejected`](super::MrfConfig::rejected).
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

/// The matcher of one configured list value. A value that is not an array
/// is warned about and reads as the empty list.
fn matcher_for(list: List, value: &Value) -> DomainMatcher {
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

/// The domain-list policy: Pleroma's `MRF.SimplePolicy`, the chain stage
/// that filters activity by the instance that sent it.
#[derive(Clone, Debug)]
pub struct SimplePolicy {
    /// The compiled lists, shared with the configuration that built them.
    config: Arc<SimpleConfig>,
}

impl SimplePolicy {
    /// The policy over the compiled `simple` configuration `config`.
    #[must_use]
    pub const fn new(config: Arc<SimpleConfig>) -> Self {
        Self { config }
    }

    /// Why the lists refuse `activity` from `host`, `None` when they admit
    /// it. `local` is this instance's own domain, which the allowlist
    /// never excludes.
    fn refusal(&self, host: &str, local: &str, activity: &Value) -> Option<String> {
        self.domain_refusal(host, local)
            .or_else(|| self.type_refusal(host, activity))
    }

    /// The refusals every activity of the host meets: its own `reject`
    /// entry, or an `accept` allowlist that does not name it.
    fn domain_refusal(&self, host: &str, local: &str) -> Option<String> {
        let config = self.config.as_ref();
        if let Some(reason) = config.reason(List::Reject, host) {
            return Some(refusal_text(List::Reject, host, Some(reason)));
        }
        let listed = host.eq_ignore_ascii_case(local) || config.matches(List::Accept, host);
        if config.configured(List::Accept) && !listed {
            return Some(refusal_text(List::Accept, host, None));
        }
        None
    }

    /// The refusals that depend on what the activity is: `Delete` under
    /// `reject_deletes`, `Flag` under `report_removal`.
    fn type_refusal(&self, host: &str, activity: &Value) -> Option<String> {
        match activity_type(activity).as_deref()? {
            DELETE_TYPE => self.refusal_for(List::RejectDeletes, host),
            FLAG_TYPE => self.refusal_for(List::ReportRemoval, host),
            _ => None,
        }
    }

    /// The refusal `list` produces for `host`, `None` when the list does
    /// not name it.
    fn refusal_for(&self, list: List, host: &str) -> Option<String> {
        self.config
            .reason(list, host)
            .map(|reason| refusal_text(list, host, Some(reason)))
    }

    /// The rewrites the lists apply to an accepted activity: the
    /// attachments first — and `media_nsfw` reads them before
    /// `media_removal` has taken them away, so a host on both lists gets
    /// its note stripped *and* marked.
    fn rewrite(&self, mut activity: Value, host: &str) -> Value {
        self.rewrite_media(&mut activity, host);
        self.rewrite_addressing(&mut activity, host);
        activity
    }

    /// `media_removal` strips a note's `attachment` entries; `media_nsfw`
    /// marks a note that had any `sensitive`.
    fn rewrite_media(&self, activity: &mut Value, host: &str) {
        let strip = self.config.matches(List::MediaRemoval, host);
        let mark = self.config.matches(List::MediaNsfw, host);
        if !strip && !mark {
            return;
        }
        let Some(object) = note_object_mut(activity) else {
            return;
        };
        let had_media = has_attachments(object);
        if strip {
            let _gone = object.remove("attachment");
        }
        if mark && had_media {
            let _previous = object.insert("sensitive".to_owned(), Value::Bool(true));
        }
    }

    /// `federated_timeline_removal` unlists and `followers_only` limits a
    /// note's addressing, on the activity and on the object it embeds:
    /// peers spell one post's recipients in both places, this instance
    /// reads the object back when it folds the note and the activity when
    /// it would send one on.
    fn rewrite_addressing(&self, activity: &mut Value, host: &str) {
        let unlist = self.config.matches(List::FederatedTimelineRemoval, host);
        let limited = self.config.matches(List::FollowersOnly, host);
        if !unlist && !limited {
            return;
        }
        if !is_note_activity(activity) {
            return;
        }
        let followers = followers_collection(activity);
        if unlist {
            rewrite_each_document(activity, unlist_document);
        }
        if limited {
            rewrite_each_document(activity, |document| limit_document(document, &followers));
        }
    }
}

impl MrfPolicy for SimplePolicy {
    fn name(&self) -> &'static str {
        "SimplePolicy"
    }

    fn filter(&self, ctx: &MrfContext, activity: Value) -> MrfResult {
        let Some(host) = activity_host(&activity) else {
            // Nothing to compare the lists against: Pleroma's catch-all
            // clause hands the document on.
            return Ok(activity);
        };
        if let Some(reason) = self.refusal(&host, &ctx.local_domain, &activity) {
            return Err(reason);
        }
        Ok(self.rewrite(activity, &host))
    }

    fn filter_actor(&self, _ctx: &MrfContext, actor: Value) -> Value {
        let Some(host) = actor_document_host(&actor) else {
            return actor;
        };
        let mut actor = actor;
        for (list, field) in ACTOR_FIELDS {
            if !self.config.matches(list, &host) {
                continue;
            }
            if let Some(document) = actor.as_object_mut() {
                let _gone = document.remove(field);
            }
        }
        actor
    }

    fn describe(&self, transparency: &Transparency) -> Map<String, Value> {
        self.config.describe(transparency)
    }
}

/// The `[SimplePolicy]` wording one list hit produces, the entry's own
/// reason (when it has one) beside it.
fn refusal_text(list: List, host: &str, reason: Option<&Reason>) -> String {
    let relation = if list == List::Accept {
        "is not on"
    } else {
        "is on"
    };
    let suffix = reason.map_or_default(Reason::suffix);
    format!(
        "[SimplePolicy] the host {host} {relation} the `{}` list{suffix}",
        list.key()
    )
}

/// The first string of a document's `type`, which may itself be an array.
pub(super) fn activity_type(document: &Value) -> Option<String> {
    document.get("type").and_then(json::first_type)
}

/// The host of an activity's `actor` — the origin every list of this policy
/// compares against. `None` for a document that names no actor, or whose
/// actor is not an `http(s)` URL.
pub(super) fn activity_host(activity: &Value) -> Option<String> {
    reference_host(activity.get("actor"))
}

/// The host an actor document's own `id` names, `None` when the document
/// carries no parsable one.
fn actor_document_host(actor: &Value) -> Option<String> {
    reference_host(actor.get("id"))
}

/// The host a reference value points at, in any of the shapes
/// [`json::value_or_id`] reads.
fn reference_host(value: Option<&Value>) -> Option<String> {
    value
        .and_then(json::value_or_id)
        .and_then(|reference| json::http_host(&reference))
}

/// Whether the document is a `Create`/`Update` whose `object` is embedded
/// and of a note-family type: the shape the content rules speak about.
pub(super) fn is_note_activity(activity: &Value) -> bool {
    let wraps = activity_type(activity).is_some_and(|kind| WRAPPING_TYPES.contains(&kind.as_str()));
    let note = activity
        .get("object")
        .and_then(Value::as_object)
        .and_then(|object| object.get("type"))
        .and_then(json::first_type)
        .is_some_and(|kind| OBJECT_TYPES.contains(&kind.as_str()));
    wraps && note
}

/// The embedded note object of such an activity, mutably; nothing else.
pub(super) fn note_object_mut(activity: &mut Value) -> Option<&mut Map<String, Value>> {
    if !is_note_activity(activity) {
        return None;
    }
    activity.get_mut("object").and_then(Value::as_object_mut)
}

/// Whether a note carries attachments: a `null` and an empty array are
/// none, anything else — one entry or many — is at least one.
pub(super) fn has_attachments(object: &Map<String, Value>) -> bool {
    object
        .get("attachment")
        .is_some_and(|value| !value.is_null() && !json::as_array(value).is_empty())
}

/// The author's followers collection: an address the documents already
/// carry when one names a `/followers` URL on the actor's own host, and the
/// `{actor}/followers` convention otherwise — the spelling Pleroma,
/// Mastodon and this instance's own actors use, and the one
/// [`parse_note`](crate::federation::note_parser::parse_note) compares the
/// author's stored `followers_url` against.
pub(super) fn followers_collection(activity: &Value) -> String {
    let actor = activity
        .get("actor")
        .and_then(json::value_or_id)
        .unwrap_or_default();
    let host = json::http_host(&actor);
    addressed_followers(activity, host.as_deref())
        .unwrap_or_else(|| format!("{actor}{FOLLOWERS_SUFFIX}"))
}

/// The followers-collection address of `host` the activity or its embedded
/// object already addresses.
fn addressed_followers(activity: &Value, host: Option<&str>) -> Option<String> {
    [activity, activity.get("object")?]
        .into_iter()
        .flat_map(|document| {
            ADDRESS_FIELDS
                .iter()
                .flat_map(move |field| document.get(*field).map_or_else(Vec::new, json::references))
        })
        .find(|address| is_followers_of(address, host))
}

/// Whether `address` is a `/followers` collection URL on `host`.
fn is_followers_of(address: &str, host: Option<&str>) -> bool {
    address.ends_with(FOLLOWERS_SUFFIX)
        && host.is_some_and(|wanted| json::http_host(address).is_some_and(|found| found == wanted))
}

/// Applies one document-level rewrite to the embedded object and then to
/// the activity itself: the two places a peer spells a note's addressing.
pub(super) fn rewrite_each_document<F>(activity: &mut Value, mut rewrite: F)
where
    F: FnMut(&mut Map<String, Value>),
{
    if let Some(object) = activity.get_mut("object").and_then(Value::as_object_mut) {
        rewrite(object);
    }
    if let Some(root) = activity.as_object_mut() {
        rewrite(root);
    }
}

/// Moves the public address out of a document's `to` and into its `cc`: the
/// note becomes unlisted. A document whose `to` names nothing public is
/// left alone — there is no timeline to remove it from.
pub(super) fn unlist_document(document: &mut Map<String, Value>) {
    if !addresses_public(document, "to") {
        return;
    }
    remove_public(document, "to");
    add_address(document, "cc", PUBLIC_ADDRESS);
}

/// Takes the public address out of a document's `to` and `cc` and puts the
/// author's followers collection in `to`: the note becomes followers-only.
/// A note nobody public was addressed (a direct one) keeps its recipients:
/// widening a private post to the whole follower set is not what limiting
/// it means.
pub(super) fn limit_document(document: &mut Map<String, Value>, followers: &str) {
    let public = addresses_public(document, "to") || addresses_public(document, "cc");
    if !public {
        return;
    }
    remove_public(document, "to");
    remove_public(document, "cc");
    add_address(document, "to", followers);
}

/// Whether `field` of `document` names the public address.
pub(super) fn addresses_public(document: &Map<String, Value>, field: &str) -> bool {
    document
        .get(field)
        .is_some_and(|value| json::as_array(value).iter().any(is_public_address))
}

/// Drops every spelling of the public address from `field`. A field given
/// as a single value is first normalized into the one-element array it
/// stands for, so the bare `"to": "…#Public"` spelling goes empty rather
/// than staying behind.
pub(super) fn remove_public(document: &mut Map<String, Value>, field: &str) {
    let Some(value) = document.get_mut(field) else {
        return;
    };
    if !value.is_array() {
        *value = Value::Array(json::as_array(value).to_vec());
    }
    if let Value::Array(entries) = value {
        entries.retain(|entry| !is_public_address(entry));
    }
}

/// Drops one exact reference from `field`, normalizing a single-valued
/// field into the one-element array it stands for, as
/// [`remove_public`] does for the public address.
pub(super) fn remove_reference(document: &mut Map<String, Value>, field: &str, address: &str) {
    let Some(value) = document.get_mut(field) else {
        return;
    };
    if !value.is_array() {
        *value = Value::Array(json::as_array(value).to_vec());
    }
    if let Value::Array(entries) = value {
        entries.retain(|entry| json::value_or_id(entry).as_deref() != Some(address));
    }
}

/// Adds `address` to one document's `field` unless it already names it,
/// creating the field as an array when the document carries none.
pub(super) fn add_address(document: &mut Map<String, Value>, field: &str, address: &str) {
    let mut entries = document
        .get(field)
        .map_or_else(Vec::new, |value| json::as_array(value).to_vec());
    if entries
        .iter()
        .any(|entry| json::value_or_id(entry).as_deref() == Some(address))
    {
        return;
    }
    entries.push(Value::String(address.to_owned()));
    let _previous = document.insert(field.to_owned(), Value::Array(entries));
}

/// Whether an addressing entry is the public address in any of the
/// [`PUBLIC_RECIPIENTS`] spellings, reference-object form included.
fn is_public_address(entry: &Value) -> bool {
    json::value_or_id(entry).is_some_and(|address| PUBLIC_RECIPIENTS.contains(&address.as_str()))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::{
        federation::{mrf::MrfConfig, note_parser},
        models::{
            status::Visibility,
            user::{self, Flag, NewUser},
        },
    };

    /// The domain every peer fixture is delivered by.
    const PEER: &str = "peer.example";
    /// This instance's own domain, as the context carries it.
    const LOCAL: &str = "test.example";
    /// The actor the fixtures speak as.
    const ALICE: &str = "https://peer.example/users/alice";
    /// That actor's followers collection, the `{actor}/followers`
    /// convention the policy falls back to.
    const FOLLOWERS: &str = "https://peer.example/users/alice/followers";
    /// The public address, full `ActivityStreams` spelling.
    const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
    /// A second recipient the fixtures keep in `cc`.
    const BOB: &str = "https://remote.example/users/bob";
    /// The note a fixture `Create` announces.
    const NOTE: &str = "https://peer.example/objects/1";

    /// The policy over the `simple` lists `lists` configures.
    fn policy(lists: &Value) -> SimplePolicy {
        SimplePolicy::new(Arc::new(SimpleConfig::from_params(Some(lists))))
    }

    /// The context of an instance whose own domain is [`LOCAL`], over the
    /// same configuration the policy under test was built from. The
    /// per-actor facts stay unloaded: `simple` reads none.
    fn ctx(lists: &Value) -> MrfContext {
        MrfContext {
            config: Arc::new(MrfConfig::assemble(
                vec![PARAMS_KEY.to_owned()],
                Map::from_iter([(PARAMS_KEY.to_owned(), lists.clone())]),
            )),
            now: Utc::now(),
            local_domain: LOCAL.to_owned(),
            author_follower_count: None,
            author_status_count: None,
            author_tags: Vec::new(),
            target_tags: Vec::new(),
        }
    }

    /// One policy run: the answer for `activity` under the lists `lists`
    /// configure.
    fn filtered(lists: &Value, activity: Value) -> MrfResult {
        let ctx = ctx(lists);
        policy(lists).filter(&ctx, activity)
    }

    /// The accepted form of `activity`, with the rejection as the failure
    /// message when the policy refused it.
    fn accepted(lists: &Value, activity: Value) -> Value {
        match filtered(lists, activity) {
            Ok(value) => value,
            Err(reason) => panic!("the policy refused the fixture: {reason}"),
        }
    }

    /// One actor-document run through the chain's actor stage.
    fn filtered_actor(lists: &Value, actor: Value) -> Value {
        let ctx = ctx(lists);
        policy(lists).filter_actor(&ctx, actor)
    }

    /// A public `Create` of a note by an account on `origin`: `to` names
    /// the public address, `cc` the author's followers and one mention, on
    /// the activity and on the embedded object alike.
    fn create_from(origin: &str) -> Value {
        let actor = format!("https://{origin}/users/alice");
        let followers = format!("{actor}/followers");
        json!({
            "id": format!("https://{origin}/activities/1"),
            "type": "Create",
            "actor": actor,
            "to": [PUBLIC],
            "cc": [followers, BOB],
            "object": {
                "id": format!("https://{origin}/objects/1"),
                "type": "Note",
                "attributedTo": actor,
                "to": [PUBLIC],
                "cc": [followers, BOB],
                "content": "<p>hello</p>",
            },
        })
    }

    /// The fixture `Create`, by `alice@peer.example`.
    fn create() -> Value {
        create_from(PEER)
    }

    /// The same `Create` with one image attachment on the note.
    fn create_with_media() -> Value {
        let mut activity = create();
        let _previous = activity["object"]
            .as_object_mut()
            .expect("the fixture embeds its note")
            .insert("attachment".to_owned(), json!([{"type": "Document", "mediaType": "image/png", "url": format!("https://{PEER}/media/1.png")}]));
        activity
    }

    /// A `Delete` of the fixture note by alice.
    fn delete() -> Value {
        json!({
            "id": format!("https://{PEER}/activities/2"),
            "type": "Delete",
            "actor": ALICE,
            "object": NOTE,
        })
    }

    /// A `Flag` (a report) by alice.
    fn flag() -> Value {
        json!({
            "id": format!("https://{PEER}/activities/3"),
            "type": "Flag",
            "actor": ALICE,
            "object": [format!("https://{PEER}/objects/spam")],
        })
    }

    /// A `Follow` by alice: an activity the note rules must never touch.
    fn follow() -> Value {
        json!({
            "id": format!("https://{PEER}/activities/4"),
            "type": "Follow",
            "actor": ALICE,
            "object": format!("https://{LOCAL}/users/carol"),
        })
    }

    /// An actor document with the three images the removal lists strip.
    fn person(origin: &str) -> Value {
        json!({
            "id": format!("https://{origin}/users/alice"),
            "type": "Person",
            "preferredUsername": "alice",
            "inbox": format!("https://{origin}/users/alice/inbox"),
            "icon": {"type": "Image", "mediaType": "image/png", "url": format!("https://{origin}/alice.png")},
            "image": {"type": "Image", "url": format!("https://{origin}/banner.png")},
            "background": {"type": "Image", "url": format!("https://{origin}/bg.png")},
        })
    }

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

    /// [`List::Reject`]: the host it names is refused, reason and all, and
    /// a host below a wildcard with it.
    #[test]
    fn the_reject_list_refuses_everything_a_matching_host_sends() {
        let named = json!({"reject": [["peer.example", "bots"]]});
        for activity in [create(), delete(), flag(), follow()] {
            assert_eq!(
                filtered(&named, activity.clone()),
                Err(format!(
                    "[SimplePolicy] the host {PEER} is on the `reject` list (bots)"
                )),
                "a `reject`ed host sends nothing, not even a deletion: {:?}",
                activity["type"]
            );
        }
        assert_eq!(
            filtered(&json!({"reject": ["*.example"]}), create()),
            Err(format!(
                "[SimplePolicy] the host {PEER} is on the `reject` list"
            )),
            "the wildcard covers the peer, and an entry without a reason says nothing"
        );
        assert_eq!(
            filtered(&json!({"reject": ["other.example"]}), create()),
            Ok(create()),
            "another host is untouched"
        );
    }

    /// [`List::Accept`]: once the allowlist holds anything, only its hosts
    /// — and this instance itself — get through.
    #[test]
    fn the_accept_list_federates_with_nobody_but_itself() {
        let lists = json!({"accept": ["friend.example", "*.partner.example"]});
        assert_eq!(
            filtered(&lists, create()),
            Err(format!(
                "[SimplePolicy] the host {PEER} is not on the `accept` list"
            )),
            "a host the allowlist does not name is refused"
        );
        assert_eq!(
            filtered(&lists, create_from("cdn.partner.example")),
            Ok(create_from("cdn.partner.example")),
            "a wildcard of the allowlist admits what is below it, unchanged"
        );
        assert_eq!(
            filtered(&lists, create_from(LOCAL)),
            Ok(create_from(LOCAL)),
            "and this instance's own domain is never excluded by it"
        );
        assert_eq!(
            filtered(&json!({"accept": []}), create()),
            Ok(create()),
            "an empty allowlist is no allowlist at all"
        );
    }

    /// [`List::RejectDeletes`]: deletions go, the rest of the host's
    /// traffic stays.
    #[test]
    fn reject_deletes_drops_only_deletions() {
        let lists = json!({"reject_deletes": ["peer.example"]});
        assert_eq!(
            filtered(&lists, delete()),
            Err(format!(
                "[SimplePolicy] the host {PEER} is on the `reject_deletes` list"
            )),
            "the deletion of a host on the list is refused"
        );
        for (kind, activity) in [("Create", create()), ("Flag", flag()), ("Follow", follow())] {
            assert_eq!(
                filtered(&lists, activity.clone()),
                Ok(activity),
                "everything but a deletion from that host passes: {kind}"
            );
        }
    }

    /// [`List::ReportRemoval`]: reports go, the rest of the host's traffic
    /// stays.
    #[test]
    fn report_removal_drops_only_reports() {
        let lists = json!({"report_removal": [["peer.example", "spam reports"]]});
        assert_eq!(
            filtered(&lists, flag()),
            Err(format!(
                "[SimplePolicy] the host {PEER} is on the `report_removal` list (spam reports)"
            )),
            "a flag from a host on the list is refused, with the operator's reason"
        );
        assert_eq!(
            filtered(&lists, create()),
            Ok(create()),
            "and its notes are still folded"
        );
    }

    /// [`List::FederatedTimelineRemoval`]: the public address moves from
    /// `to` to `cc`, on the activity and on the note it embeds, and
    /// nothing else about the document changes.
    #[test]
    fn federated_timeline_removal_unlists_a_note() {
        let lists = json!({"federated_timeline_removal": ["peer.example"]});
        let expected = json!({
            "id": format!("https://{PEER}/activities/1"),
            "type": "Create",
            "actor": ALICE,
            "to": [],
            "cc": [FOLLOWERS, BOB, PUBLIC],
            "object": {
                "id": NOTE,
                "type": "Note",
                "attributedTo": ALICE,
                "to": [],
                "cc": [FOLLOWERS, BOB, PUBLIC],
                "content": "<p>hello</p>",
            },
        });

        assert_eq!(
            accepted(&lists, create()),
            expected,
            "the note leaves the public timeline"
        );
        assert_eq!(
            accepted(&lists, follow()),
            follow(),
            "a `Follow` from the same host carries no note and is left alone"
        );
    }

    /// [`List::FollowersOnly`]: the public address goes and the author's
    /// followers collection takes its place in `to`; a note nobody public
    /// was addressed keeps the recipients it had.
    #[test]
    fn followers_only_limits_a_public_note_to_its_followers() {
        let lists = json!({"followers_only": ["peer.example"]});
        let expected = json!({
            "id": format!("https://{PEER}/activities/1"),
            "type": "Create",
            "actor": ALICE,
            "to": [FOLLOWERS],
            "cc": [FOLLOWERS, BOB],
            "object": {
                "id": NOTE,
                "type": "Note",
                "attributedTo": ALICE,
                "to": [FOLLOWERS],
                "cc": [FOLLOWERS, BOB],
                "content": "<p>hello</p>",
            },
        });

        assert_eq!(
            accepted(&lists, create()),
            expected,
            "public gone, followers in `to`"
        );

        let direct = json!({
            "id": format!("https://{PEER}/activities/5"),
            "type": "Create",
            "actor": ALICE,
            "to": [BOB],
            "object": {"id": NOTE, "type": "Note", "attributedTo": ALICE, "to": [BOB]},
        });
        assert_eq!(
            accepted(&lists, direct.clone()),
            direct,
            "a direct note is not widened into a followers post"
        );
    }

    /// A followers-only note whose author is the stored mirror parses as
    /// `private`, and an unlisted one as `unlisted`: the rewrites are what
    /// this instance reads back, not only what it sends on.
    #[tokio::test]
    async fn the_addressing_rewrites_read_back_as_their_visibility() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let author = stored_author(db.pool()).await;

        let limited = accepted(&json!({"followers_only": [PEER]}), create());
        let parsed = note_parser::parse_note(object_of(&limited), &author, Utc::now())
            .expect("the rewritten note parses");
        assert_eq!(
            parsed.status.visibility,
            Visibility::Private,
            "followers-only is what the parser sees"
        );

        let unlisted = accepted(&json!({"federated_timeline_removal": [PEER]}), create());
        let relaxed = note_parser::parse_note(object_of(&unlisted), &author, Utc::now())
            .expect("the rewritten note parses");
        assert_eq!(
            relaxed.status.visibility,
            Visibility::Unlisted,
            "and the timeline removal reads as unlisted"
        );
    }

    /// [`List::MediaRemoval`] drops a note's attachments and nothing else.
    #[test]
    fn media_removal_strips_a_notes_attachments() {
        let lists = json!({"media_removal": ["peer.example"]});
        let expected = json!({
            "id": format!("https://{PEER}/activities/1"),
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
                "content": "<p>hello</p>",
            },
        });

        assert_eq!(
            accepted(&lists, create_with_media()),
            expected,
            "the note stands without its attachment"
        );
        assert_eq!(
            accepted(&lists, create()),
            create(),
            "a note without attachments is already what it should be"
        );
    }

    /// [`List::MediaNsfw`] marks a note that has attachments sensitive —
    /// one it already was, and nothing a note without media.
    #[test]
    fn media_nsfw_marks_notes_that_carry_media() {
        let lists = json!({"media_nsfw": ["peer.example"]});
        let expected = {
            let mut marked = create_with_media();
            marked["object"]["sensitive"] = json!(true);
            marked
        };

        assert_eq!(
            accepted(&lists, create_with_media()),
            expected,
            "the attachment is flagged as the policy asks"
        );
        assert_eq!(
            accepted(&lists, create()),
            create(),
            "a note with no media gets no flag"
        );
        let both = json!({"media_nsfw": [PEER], "media_removal": [PEER]});
        let stripped = accepted(&both, create_with_media());
        assert_eq!(
            stripped["object"]["sensitive"],
            json!(true),
            "with both lists on, the note loses its media and gains the flag"
        );
        assert!(
            stripped["object"].get("attachment").is_none(),
            "the removal still happens"
        );
    }

    /// The actor stage: `avatar_removal`, `banner_removal` and
    /// `background_removal` take the named field out of a matching host's
    /// document, each on its own.
    #[test]
    fn the_actor_lists_strip_their_fields_from_a_profile() {
        let avatar = filtered_actor(&json!({"avatar_removal": [PEER]}), person(PEER));
        assert!(avatar.get("icon").is_none(), "the avatar is gone: {avatar}");
        assert_eq!(avatar["image"], person(PEER)["image"], "the banner stays");
        assert_eq!(
            avatar["background"],
            person(PEER)["background"],
            "and the background"
        );

        let banner = filtered_actor(&json!({"banner_removal": ["*.example"]}), person(PEER));
        assert!(
            banner.get("image").is_none(),
            "a wildcard covers the host: {banner}"
        );
        assert_eq!(
            banner["icon"],
            person(PEER)["icon"],
            "and only its own field goes"
        );

        let background = filtered_actor(
            &json!({"background_removal": [[PEER, "lewd headers"]]}),
            person(PEER),
        );
        assert!(
            background.get("background").is_none(),
            "the background is gone: {background}"
        );

        assert_eq!(
            filtered_actor(&json!({"avatar_removal": ["other.example"]}), person(PEER)),
            person(PEER),
            "another host keeps its profile images"
        );
        let no_lists = filtered_actor(&json!({}), person(PEER));
        assert_eq!(
            no_lists,
            person(PEER),
            "and with nothing configured nothing goes"
        );
    }

    /// A document the lists have nothing to say about passes through
    /// unchanged — including one that names no actor at all.
    #[test]
    fn a_document_the_lists_do_not_name_passes_through() {
        let lists = json!({"reject": ["other.example"], "media_removal": ["other.example"]});
        for activity in [
            create(),
            follow(),
            delete(),
            json!({"type": "Note", "content": "an object, not an activity"}),
            json!({"id": format!("https://{PEER}/activities/9"), "type": "Create", "object": NOTE}),
        ] {
            assert_eq!(
                filtered(&lists, activity.clone()),
                Ok(activity),
                "an unconfigured host and an actor-less document both pass"
            );
        }
    }

    /// The policy is Pleroma's `SimplePolicy`, and its transparency blocks
    /// are `mrf_simple` — all eleven lists, the configured ones holding
    /// their patterns verbatim — beside `mrf_simple_info`, the reasons the
    /// entries carry.
    #[test]
    fn the_policy_names_itself_and_describes_its_lists() {
        let lists = json!({
            "reject": [["evil.example", "bots"], "*.worse.example"],
            "accept": ["friend.example"],
        });
        let policy = policy(&lists);

        assert_eq!(policy.name(), "SimplePolicy", "Pleroma's module name");
        let described = policy.describe(&Transparency::open());
        assert_eq!(
            described["mrf_simple"]["reject"],
            json!(["evil.example", "*.worse.example"]),
            "the patterns come out as configured, wildcard verbatim, in order"
        );
        assert_eq!(
            described["mrf_simple"]["accept"],
            json!(["friend.example"]),
            "the allowlist is published too"
        );
        assert_eq!(
            described["mrf_simple_info"],
            json!({"reject": {"evil.example": {"reason": "bots"}}}),
            "the reasons ride in the separate object Pleroma keeps them in, \
             for the one list whose entries carry one"
        );
        let published = described["mrf_simple"]
            .as_object()
            .expect("the block is an object");
        assert_eq!(
            published.len(),
            List::ALL.len(),
            "every list is published, empty ones as the empty array, the way \
             Pleroma maps its whole `:mrf_simple` config"
        );
        for list in List::ALL {
            assert!(
                published.contains_key(list.key()),
                "`{}` names one of the published keys",
                list.key()
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

    /// The note rules speak about an `Update` as they do about a `Create`,
    /// and the followers collection they write is the one the document
    /// already addresses — even in its reference-object spelling, whose
    /// string form is then not duplicated.
    #[test]
    fn an_update_is_rewritten_like_a_create_and_the_documents_own_followers_url_wins() {
        let lists = json!({"followers_only": [PEER], "media_removal": [PEER]});
        let update = json!({
            "id": format!("https://{PEER}/activities/6"),
            "type": "Update",
            "actor": {"id": ALICE, "type": "Person"},
            "to": [{"type": "Collection", "id": FOLLOWERS}, PUBLIC],
            "object": {
                "id": NOTE,
                "type": "Note",
                "attributedTo": ALICE,
                "to": [PUBLIC],
                "cc": [{"type": "OrderedCollection", "id": FOLLOWERS}],
                "attachment": [{"type": "Document", "url": format!("https://{PEER}/media/2.png")}],
            },
        });

        let rewritten = accepted(&lists, update);

        assert_eq!(
            rewritten["to"],
            json!([{"type": "Collection", "id": FOLLOWERS}]),
            "the activity keeps the collection it addressed and loses the public one"
        );
        assert_eq!(
            rewritten["object"]["to"],
            json!([FOLLOWERS]),
            "the note gains the very same followers URL"
        );
        assert_eq!(
            rewritten["object"]["cc"],
            json!([{"type": "OrderedCollection", "id": FOLLOWERS}]),
            "its reference-object entries are otherwise untouched"
        );
        assert!(
            rewritten["object"].get("attachment").is_none(),
            "and the media rule fired on the `Update` too"
        );
    }

    /// The stored mirror of the fixture author, with the followers
    /// collection the policy's fallback spells the same way.
    async fn stored_author(pool: &PgPool) -> user::User {
        user::insert(
            pool,
            &NewUser {
                nickname: "alice@peer.example".to_owned(),
                ap_id: ALICE.to_owned(),
                local: Flag::new(false),
                domain: Some(PEER.to_owned()),
                followers_url: Some(FOLLOWERS.to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the author fixture inserts")
    }

    /// The embedded note of a `Create` fixture.
    fn object_of(activity: &Value) -> &Value {
        activity.get("object").expect("the fixture embeds its note")
    }
}
