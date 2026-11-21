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
//! through the operator's [`Transparency`](super::Transparency), which
//! keeps the hosts of
//! `MRF_TRANSPARENCY_EXCLUSIONS` out of public output while they stay on
//! the list and in force.
//!
//! The module is a tree: the lists as configured — [`List`], [`Reason`] and
//! [`Entry`] — live in `lists`, the compiled matching in `matcher`, the
//! configuration read in `config`, the chain stage in `policy` and the
//! document helpers the other policies share in `rewrite`; this root keeps
//! the constants and the policy itself and re-exports the whole
//! outside-facing surface.

use std::sync::Arc;

mod config;
mod lists;
mod matcher;
mod policy;
mod rewrite;
#[cfg(test)]
mod test_support;

pub use config::SimpleConfig;
pub use lists::{Entry, List, Reason};
pub use matcher::{DomainMatcher, Publication};

pub(super) use rewrite::{
    activity_host, activity_type, add_address, addresses_public, followers_collection,
    has_attachments, is_note_activity, limit_document, note_object_mut, remove_public,
    remove_reference, rewrite_each_document, unlist_document,
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
/// ([`PUBLIC_RECIPIENTS`](crate::federation::note_parser::PUBLIC_RECIPIENTS)
/// covers the two shorthands it may arrive as).
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

/// The domain-list policy: Pleroma's `MRF.SimplePolicy`, the chain stage
/// that filters activity by the instance that sent it.
#[derive(Clone, Debug)]
pub struct SimplePolicy {
    /// The compiled lists, shared with the configuration that built them.
    config: Arc<SimpleConfig>,
}
