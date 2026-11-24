//! The two small views the instance endpoints serve beside the documents:
//! the domain blocks the `simple` MRF policy publishes, and the extended
//! description, plus the `SHA-256` digest the domain blocks carry.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::{
    config::Config,
    federation::mrf::{
        Transparency,
        simple::{self, SimpleConfig},
    },
    util::{time::mastodon_time, token::sha256_hex},
};

/// One entry of `GET /api/v1/instance/domain_blocks`: a domain the
/// [`simple`](crate::federation::mrf::simple) policy restricts, in the
/// shape Mastodon's domain-block listing documents.
///
/// The `digest` lets a client check the block without learning the
/// domain (Mastodon hashes it the same way); this instance serves the
/// domain in the clear anyway — it stands in the transparency lists too —
/// so both are in the entry.
#[derive(Debug, Serialize)]
pub(super) struct DomainBlock {
    /// The blocked host, verbatim from the configured pattern.
    domain: String,
    /// Lowercase hex `SHA-256` of [`domain`](Self::domain).
    digest: String,
    /// How the block reads to clients: `suspend` or `silence`
    /// ([`DomainBlock::severity_of`]).
    severity: &'static str,
    /// Why the domain is blocked, empty when the operator gave no reason.
    comment: String,
}

impl DomainBlock {
    /// The entry for one published list `pattern` beside its `reason`, at
    /// `severity`: Pleroma's renderer hashes the host it prints and
    /// carries the entry's reason as the comment.
    fn new(pattern: &str, reason: &str, severity: &'static str) -> Self {
        Self {
            digest: sha256_hex(pattern),
            domain: pattern.to_owned(),
            severity,
            comment: reason.to_owned(),
        }
    }

    /// Every block the instance publishes from the `simple` domain `lists`
    /// (the answer of [`MrfConfig::simple`]): one entry per host of each
    /// list whose effect a client reads as a domain block, in
    /// [`simple::List::ALL`] order — so the rejected hosts come first and
    /// the softened ones follow, each group in the order it was
    /// configured.
    ///
    /// The empty array comes back twice over: while transparency is off
    /// (nothing of the instance's moderation is published, this listing
    /// included, exactly as Pleroma's renderer answers `[]`), and for an
    /// instance that does not run the `simple` policy at all — `None`
    /// lists, which govern no traffic and restrict no domain.
    ///
    /// [`MrfConfig::simple`]: crate::federation::mrf::MrfConfig::simple
    pub(super) fn published_all(
        lists: Option<&SimpleConfig>,
        transparency: &Transparency,
    ) -> Vec<Self> {
        if !transparency.enabled() {
            return Vec::new();
        }
        let Some(lists) = lists else {
            return Vec::new();
        };
        simple::List::ALL
            .iter()
            .filter_map(|list| Self::for_list(*list, lists, transparency))
            .flatten()
            .collect()
    }

    /// The entries of one `list`, `None` when the list restricts nothing a
    /// domain-block listing shows: the allowlist, the delete and report
    /// gates and the three profile strips all act on traffic a client
    /// never sees addressed to a blocked domain.
    fn for_list(
        list: simple::List,
        lists: &SimpleConfig,
        transparency: &Transparency,
    ) -> Option<Vec<Self>> {
        let severity = Self::severity_of(list)?;
        Some(
            lists
                .published(list, transparency)
                .iter()
                .map(|entry| Self::new(entry.pattern, entry.reason, severity))
                .collect(),
        )
    }

    /// The Mastodon severity a `simple` list maps to: a rejected domain is
    /// suspended; the four lists that keep a host's posts out of the
    /// public eye — off the federated timeline, behind the author's
    /// followers, stripped of attachments, marked sensitive — all read as
    /// `silence` (the ticket's mapping; Pleroma likewise folds its soft
    /// blocks into one severity, listing only two of these lists).
    const fn severity_of(list: simple::List) -> Option<&'static str> {
        match list {
            simple::List::Reject => Some("suspend"),
            simple::List::FederatedTimelineRemoval
            | simple::List::FollowersOnly
            | simple::List::MediaRemoval
            | simple::List::MediaNsfw => Some("silence"),
            simple::List::Accept
            | simple::List::RejectDeletes
            | simple::List::ReportRemoval
            | simple::List::AvatarRemoval
            | simple::List::BannerRemoval
            | simple::List::BackgroundRemoval => None,
        }
    }
}

/// `GET /api/v1/instance/extended_description`: Mastodon's long-form
/// description, stored as rich text with a revision date. wired-be has
/// the one [`Config::instance_description`] and no revisions, so
/// [`ExtendedDescription::build`] wraps it in a paragraph and stamps it
/// with the request time.
#[derive(Debug, Serialize)]
pub(super) struct ExtendedDescription {
    /// When the description last changed ([`ExtendedDescription::build`]).
    updated_at: String,
    /// The description as an HTML paragraph.
    content: String,
}

impl ExtendedDescription {
    /// The document for `config`'s description, stamped `now` in
    /// Mastodon's time format (decision 5).
    pub(super) fn build(config: &Config, now: DateTime<Utc>) -> Self {
        Self {
            updated_at: mastodon_time(&now),
            content: format!("<p>{}</p>", config.instance_description),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::api::instance::test_support::stamped;

    #[test]
    fn domain_block_severities_follow_the_ticket_mapping() {
        assert_eq!(
            DomainBlock::severity_of(simple::List::Reject),
            Some("suspend"),
            "a rejected domain reads as suspended"
        );
        for list in [
            simple::List::FederatedTimelineRemoval,
            simple::List::FollowersOnly,
            simple::List::MediaRemoval,
            simple::List::MediaNsfw,
        ] {
            assert_eq!(
                DomainBlock::severity_of(list),
                Some("silence"),
                "the list that keeps a host's posts out of the public eye reads \
                 as silence: {list:?}"
            );
        }
        for list in [
            simple::List::Accept,
            simple::List::RejectDeletes,
            simple::List::ReportRemoval,
            simple::List::AvatarRemoval,
            simple::List::BannerRemoval,
            simple::List::BackgroundRemoval,
        ] {
            assert_eq!(
                DomainBlock::severity_of(list),
                None,
                "the list that blocks no addressed domain renders no entry: {list:?}"
            );
        }
    }

    /// The listing is the `simple` lists: one entry per published host, at
    /// its list's severity, the pattern verbatim — wildcard and all — and
    /// the operator's reason as the comment.
    #[test]
    fn a_domain_block_holds_the_domain_digest_severity_and_comment() {
        let lists = SimpleConfig::from_params(Some(&json!({
            "reject": [["evil.example", "coordinated abuse"], "*.worse.example"],
            "media_nsfw": ["lewd.example"],
            "followers_only": ["press.example"],
            "avatar_removal": ["faceless.example"],
        })));
        let rendered = serde_json::to_value(DomainBlock::published_all(
            Some(&lists),
            &Transparency::open(),
        ))
        .unwrap();
        assert_eq!(
            rendered,
            json!([
                {
                    "domain": "evil.example",
                    "digest": sha256_hex("evil.example"),
                    "severity": "suspend",
                    "comment": "coordinated abuse",
                },
                {
                    "domain": "*.worse.example",
                    "digest": sha256_hex("*.worse.example"),
                    "severity": "suspend",
                    "comment": "",
                },
                {
                    "domain": "press.example",
                    "digest": sha256_hex("press.example"),
                    "severity": "silence",
                    "comment": "",
                },
                {
                    "domain": "lewd.example",
                    "digest": sha256_hex("lewd.example"),
                    "severity": "silence",
                    "comment": "",
                },
            ]),
            "reject first, then the softened lists in `List::ALL` order, each \
             entry with its digest and (empty) comment; the profile strip \
             renders nothing"
        );
    }

    /// The two ways the listing comes back empty: the instance publishes
    /// no moderation at all, and the instance runs no `simple` policy, so
    /// its configured lists govern nothing.
    #[test]
    fn without_transparency_or_simple_lists_nothing_is_blocked() {
        let lists = SimpleConfig::from_params(Some(&json!({"reject": ["evil.example"]})));
        assert!(
            DomainBlock::published_all(Some(&lists), &Transparency::default()).is_empty(),
            "MRF_TRANSPARENCY off publishes no domain blocks either"
        );
        assert!(
            DomainBlock::published_all(None, &Transparency::open()).is_empty(),
            "and no `simple` policy means no blocked domains"
        );
    }

    /// An excluded host is missing from the listing, and only it: the rest
    /// of the same list is still published.
    #[test]
    fn an_excluded_host_is_missing_from_the_listing() {
        let lists = SimpleConfig::from_params(Some(&json!({
            "reject": ["evil.example", "press.example"],
        })));
        let transparency = Transparency::hiding(&["press.example".to_owned()]);
        let rendered =
            serde_json::to_value(DomainBlock::published_all(Some(&lists), &transparency)).unwrap();
        assert_eq!(
            rendered,
            json!([
                {
                    "domain": "evil.example",
                    "digest": sha256_hex("evil.example"),
                    "severity": "suspend",
                    "comment": "",
                },
            ]),
            "the excluded host is simply not there"
        );
    }

    #[test]
    fn the_extended_description_wraps_the_config_text() {
        let config = Config::for_tests();
        let now = stamped();
        let description = ExtendedDescription::build(&config, now);
        let rendered = serde_json::to_value(&description).unwrap();
        assert_eq!(
            rendered,
            json!({
                "updated_at": "2026-10-11T12:00:00.000Z",
                "content": "<p>A Wired instance, compatible with Pleroma clients</p>",
            }),
            "the paragraph-wrapped INSTANCE_DESCRIPTION stamped in Mastodon time"
        );
    }
}
