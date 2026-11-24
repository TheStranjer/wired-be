//! The moderation/filter pipeline (MRF): the policy chain every activity
//! passes before it is stored or sent.
//!
//! The reference is Pleroma's `Pleroma.Web.ActivityPub.MRF`
//! (`lib/pleroma/web/activity_pub/mrf.ex`): every activity the federator
//! received passes a chain of filters, each of which may rewrite the
//! document or reject it outright, and every activity the federator sends
//! passes the same chain on its way out. Here the chain is a
//! [`PolicyChain`] of [`MrfPolicy`] trait objects, assembled from the
//! instance's [`MrfConfig`] by [`build_chain`]; [`filter_incoming`] and
//! [`filter_outgoing`] run one document through it and answer with an
//! [`MrfDecision`].
//!
//! Policies are synchronous and perform no IO: everything a policy can
//! consult is preloaded into the [`MrfContext`] — the configuration
//! document, one shared "now", and the local domain — before the chain
//! runs. The configuration itself comes from the admin config store: the
//! `:mrf` entry's fields with the `:mrf_simple`, `:mrf_keyword`,
//! `:mrf_hellthread`, `:mrf_object_age` and `:mrf_hashtag` entries under
//! their policy keys (see [`crate::services::runtime_config`]), composed by
//! [`RuntimeConfig::mrf_document`](crate::services::runtime_config::RuntimeConfig::mrf_document),
//! with the `MRF_POLICIES` environment
//! variable (a comma list) as the fallback for deployments that enable
//! policies without any stored row; reads are cached in memory for
//! [`MRF_CONFIG_TTL`], so a busy inbox costs one query per window, and
//! `POST /api/v1/pleroma/admin/config` clears that cache when a `:mrf*`
//! entry changes.
//!
//! The pipeline ships Pleroma's no-op policy
//! ([`NoOpPolicy`], `lib/pleroma/web/activity_pub/mrf/noop_policy.ex`) and
//! seven real ones: the domain lists
//! (`lib/pleroma/web/activity_pub/mrf/simple_policy.ex`, in
//! [`simple`]), the first-post link gate
//! (`lib/pleroma/web/activity_pub/mrf/anti_link_spam_policy.ex`, in
//! [`anti_link_spam`]), the markup normalizer
//! (`lib/pleroma/web/activity_pub/mrf/normalize_markup_policy.ex`, in
//! [`normalize_markup`]), the user-tag switches
//! (`lib/pleroma/web/activity_pub/mrf/tag_policy.ex`, in [`tag`]), the
//! keyword and regex filters
//! (`lib/pleroma/web/activity_pub/mrf/keyword_policy.ex`, in [`keyword`]),
//! the mention-count gate
//! (`lib/pleroma/web/activity_pub/mrf/hellthread_policy.ex`, in
//! [`hellthread`]) and the age gate
//! (`lib/pleroma/web/activity_pub/mrf/object_age_policy.ex`, in
//! [`object_age`]); further policy tickets add one `match` arm to
//! [`build_chain`] each. Unknown names are warned about and skipped, so a
//! configuration written ahead of a deploy never drops traffic.
//!
//! One more policy is not named by the configuration at all: Pleroma
//! appends its `HashtagPolicy`
//! (`lib/pleroma/web/activity_pub/mrf/hashtag_policy.ex`, in [`hashtag`]) to
//! every chain, so [`build_chain`] does too — last, after the configured
//! policies, on its default lists when the `hashtag` key is absent.
//!
//! Two of those policies read facts about the actors an activity speaks
//! about — follower and status counters, profile tags — that live in the
//! database, and policies perform no IO: the chain runner therefore
//! preloads them into [`MrfContext`]’s `author_follower_count`,
//! `author_status_count`, `author_tags` and `target_tags` fields before
//! the chain runs, one query each, and only while the policy that reads
//! them is enabled.
//!
//! Actor documents — the profiles this instance mirrors — are not
//! activities, so they run through the chain's other stage,
//! [`filter_actor`], which only the domain-list policy touches and which
//! never rejects: [`upsert_remote_user`](crate::federation::remote_actor::upsert_remote_user)'s
//! callers and the inbound `Update` of an account fold what it answers.
//!
//! The chain's shape is also public metadata: [`describe`] feeds the
//! `pleroma.metadata.federation` block of the instance endpoints and
//! `NodeInfo`, where Pleroma reports its active policies as
//! `"SimplePolicy"`-style names beside `mrf_simple` transparency objects.
//! Those objects are the operator's [`Transparency`] settings' business:
//! with `MRF_TRANSPARENCY` off the documents publish no lists and no
//! policy names at all, and every host named by
//! `MRF_TRANSPARENCY_EXCLUSIONS` is missing from the lists that are — the
//! use of that exclusion list is itself disclosed as `"exclusions": true`.
//! [`MrfConfig::simple`] and [`MrfConfig::on_reject_list`] are the same
//! configuration read by `GET /api/v1/instance/domain_blocks` and
//! `GET /api/v1/instance/peers`, which render the domain lists as
//! Mastodon's domain blocks and keep the rejected hosts out of the peer
//! list.

pub mod anti_link_spam;
pub mod hashtag;
pub mod hellthread;
pub mod keyword;
pub mod normalize_markup;
pub mod object_age;
pub mod simple;
pub mod tag;

mod chain;
mod config;
mod filter;
mod transparency;

#[cfg(test)]
mod test_support;

pub use chain::{NoOpPolicy, PolicyChain, active_chain, build_chain};
pub use config::{MRF_CONFIG_TTL, MrfConfig, MrfConfigCache, load_config};
pub use filter::{
    filter_actor, filter_incoming, filter_incoming_boxed, filter_outgoing, filter_outgoing_boxed,
    refuse_rejected,
};
pub use transparency::{
    ChainTransparency, Transparency, describe, describe_chain, describe_policies,
    describe_transparency, single_block,
};

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

/// What the policy pipeline decided about one activity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MrfDecision {
    /// The activity may proceed; the value is the document to proceed
    /// with — the original, or the chain's rewritten form of it.
    Accept(Value),
    /// The activity must not proceed; the string says why, for the
    /// caller's debug log.
    Reject(String),
}

/// One policy's answer about an activity: `Ok` carries the (possibly
/// rewritten) document onward, `Err` carries the rejection reason that
/// ends the chain.
pub type MrfResult = Result<Value, String>;

/// One stage of the [`PolicyChain`].
///
/// A policy is synchronous and IO-free: everything it may consult sits in
/// the [`MrfContext`] the chain was handed. The trait needs `Send + Sync`
/// because one chain is built per activity on whatever worker thread runs
/// the pipeline, and the instance endpoints read the same shape to
/// describe itself.
pub trait MrfPolicy: Send + Sync {
    /// The policy's name, formatted the way Pleroma reports policies in
    /// instance metadata: `"SimplePolicy"`, `"NoOpPolicy"`.
    fn name(&self) -> &'static str;

    /// Rewrites the activity (`Ok`) or rejects it (`Err` reason).
    ///
    /// # Errors
    ///
    /// The `Err` arm carries the rejection reason — the first one ends
    /// the chain, and it is what the caller's debug log reports.
    fn filter(&self, ctx: &MrfContext, activity: Value) -> MrfResult;

    /// The policy's public transparency blocks, keyed the way the
    /// federation metadata wants them — Pleroma's `MRF.describe/1` merges
    /// one such map per policy beside its own names. The default is the
    /// policy's own key holding the empty object: a policy overrides this
    /// once it has lists worth publishing, and the [`Transparency`]
    /// settings say which of those lists' hosts stay unpublished.
    fn describe(&self, _transparency: &Transparency) -> Map<String, Value> {
        single_block(self.name(), Value::Object(Map::new()))
    }

    /// Rewrites an actor document on its way into a mirror row.
    ///
    /// An actor document is not an activity, so the chain's
    /// [`filter`](Self::filter) stage never sees one: the policies that
    /// strip a profile's `icon` or `image` implement this instead. It
    /// cannot reject — a stripped profile is still a profile, and dropping
    /// the account over one avatar would strand the posts already stored
    /// under it — so the answer is the document. The default hands it back
    /// untouched.
    fn filter_actor(&self, _ctx: &MrfContext, actor: Value) -> Value {
        actor
    }
}

/// Everything a policy may consult, preloaded before the chain runs.
#[derive(Clone, Debug)]
pub struct MrfContext {
    /// The instance's MRF configuration, shared across the chain.
    pub config: Arc<MrfConfig>,
    /// The instant the chain runs: one "now" for every policy, so two age
    /// checks inside one activity cannot disagree.
    pub now: DateTime<Utc>,
    /// This instance's domain ([`Config::domain`](crate::config::Config::domain)):
    /// the "local" a policy compares activity hosts against.
    pub local_domain: String,
    /// The activity actor's `users.followers_count`, preloaded by the
    /// chain runner while `anti_link_spam` is enabled. `None` says the
    /// fact was never fetched or the actor has no row here — the
    /// [`anti_link_spam`] policy weighs both readings as a newcomer.
    pub author_follower_count: Option<i32>,
    /// The activity actor's `users.statuses_count`, preloaded beside
    /// [`MrfContext::author_follower_count`] under the same condition.
    pub author_status_count: Option<i32>,
    /// The `users.tags` of the activity's actor, preloaded while the `tag`
    /// policy is enabled; empty when the fact was not fetched, the actor
    /// has no row, or the row carries no tags.
    pub author_tags: Vec<String>,
    /// The `users.tags` of the LOCAL user a `Follow` is addressed to,
    /// preloaded while the `tag` policy is enabled; empty for any other
    /// activity, for a target with no local row, and for a row without
    /// tags.
    pub target_tags: Vec<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::federation::dispatch::{ActivityView, ObjectRef};

    /// The decision arms stay distinguishable and keep their payloads.
    #[test]
    fn the_decision_arms_carry_their_payloads() {
        let accept = MrfDecision::Accept(json!({"type": "Create"}));
        let reject = MrfDecision::Reject("domain is frozen".to_owned());
        assert_ne!(accept, reject, "accept and reject are distinct answers");
        let MrfDecision::Accept(value) = accept else {
            panic!("the accept arm keeps its value");
        };
        assert_eq!(value["type"], "Create", "and the value verbatim");
    }

    /// The view the pipeline reads is the dispatcher's normalization.
    #[test]
    fn the_pipeline_reads_the_normalized_view() {
        let raw = json!({
            "id": "https://peer.example/activities/2",
            "type": ["Create"],
            "actor": ["https://peer.example/users/alice"],
            "object": {"id": "https://peer.example/posts/1", "type": "Note"},
        });
        let view = ActivityView::parse(&raw).expect("the fixture parses");
        assert_eq!(view.kind, "Create", "the array type normalizes");
        assert!(
            matches!(view.object, ObjectRef::Embedded(_)),
            "the embedded object stays inspectable for filters"
        );
    }
}
