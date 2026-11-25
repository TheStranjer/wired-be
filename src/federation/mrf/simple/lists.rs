//! The lists as the operator configures them: [`List`], the eleven named
//! domain lists, [`Reason`], the note recorded beside one entry, and
//! [`Entry`], one configured pattern beside its reason — the JSON read of
//! the `simple` settings, which the matcher side compiles into rules.

use std::fmt;

use serde_json::Value;
use tracing::warn;

use super::{WILDCARD_PREFIX, matcher::Rule};
use crate::util::url::canonical_host;

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
    /// `media_nsfw`: mark all of matching hosts' notes sensitive, with or
    /// without attachments.
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
    pub(super) fn suffix(&self) -> String {
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
    pub(super) fn rule(&self) -> Option<Rule> {
        let pattern = self.pattern.trim().to_ascii_lowercase();
        if pattern.is_empty() {
            warn!("an `mrf.simple` entry names no domain; ignored");
            return None;
        }
        let wildcard = pattern.starts_with(WILDCARD_PREFIX);
        let stem = canonical_host(pattern.strip_prefix(WILDCARD_PREFIX).unwrap_or(&pattern));
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
