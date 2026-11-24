//! The chain itself: [`PolicyChain`], the [`build_chain`] assembly from
//! [`MrfConfig`] names, the [`NoOpPolicy`] placeholder, and the
//! [`run_chain`] path one pipeline run takes through it.

use std::{fmt, sync::Arc};

use serde_json::Value;
use tracing::warn;

use super::{
    MrfContext, MrfDecision, MrfPolicy, MrfResult, anti_link_spam,
    config::{MrfConfig, load_config},
    filter::activity_context,
    hashtag, hellthread, keyword, normalize_markup, object_age, simple, tag,
    transparency::describe_policies,
};
use crate::state::AppState;

/// The `ActivityStreams` type whose target the `tag` policy's subscription
/// rules address: the only activity shape that preloads `target_tags`.
pub(super) const FOLLOW_TYPE: &str = "Follow";

/// The chain the pipeline runs: enabled policies in configured order.
pub struct PolicyChain {
    /// The policies, in the order [`build_chain`] assembled them.
    pub(super) policies: Vec<Box<dyn MrfPolicy + Send + Sync>>,
}

impl PolicyChain {
    /// Runs one document through every policy in order. The first `Err`
    /// ends the chain and becomes the rejection; each `Ok` is the
    /// document the next policy sees, so rewrites propagate.
    #[must_use]
    pub fn apply(&self, ctx: &MrfContext, activity: Value) -> MrfDecision {
        let mut current = activity;
        for policy in &self.policies {
            match policy.filter(ctx, current) {
                Ok(next) => current = next,
                Err(reason) => return MrfDecision::Reject(reason),
            }
        }
        MrfDecision::Accept(current)
    }

    /// Runs one actor document through every policy's
    /// [`filter_actor`](MrfPolicy::filter_actor) in order: the mirror half
    /// of the chain, where rewrites propagate the same way and nothing can
    /// be rejected.
    #[must_use]
    pub fn apply_actor(&self, ctx: &MrfContext, actor: Value) -> Value {
        let mut current = actor;
        for policy in &self.policies {
            current = policy.filter_actor(ctx, current);
        }
        current
    }
}

/// Assembles the chain [`MrfConfig`] names.
///
/// Each recognized name gets its policy, each unknown name is warned
/// about and skipped — never a reason to fail an activity. Policy
/// tickets add one `match` arm here each. The always-on
/// [`HashtagPolicy`](hashtag::HashtagPolicy) comes last, as in Pleroma.
#[must_use]
pub fn build_chain(config: &MrfConfig) -> PolicyChain {
    let mut policies: Vec<Box<dyn MrfPolicy + Send + Sync>> = Vec::new();
    for name in config.policies() {
        let policy: Option<Box<dyn MrfPolicy + Send + Sync>> = match name.as_str() {
            "noop" => Some(Box::new(NoOpPolicy)),
            "simple" => Some(Box::new(simple::SimplePolicy::new(Arc::clone(
                &config.simple,
            )))),
            "anti_link_spam" => Some(Box::new(anti_link_spam::AntiLinkSpamPolicy::new(
                config.params_for(anti_link_spam::PARAMS_KEY).cloned(),
            ))),
            "normalize_markup" => Some(Box::new(normalize_markup::NormalizeMarkupPolicy::new(
                config.params_for(normalize_markup::PARAMS_KEY).cloned(),
            ))),
            "tag" => Some(Box::new(tag::TagPolicy::new(
                config.params_for(tag::PARAMS_KEY).cloned(),
            ))),
            "keyword" => Some(Box::new(keyword::KeywordPolicy::new(Arc::clone(
                &config.keyword,
            )))),
            "hellthread" => Some(Box::new(hellthread::HellthreadPolicy::new(
                config.params_for(hellthread::PARAMS_KEY).cloned(),
            ))),
            "object_age" => Some(Box::new(object_age::ObjectAgePolicy::new(
                config.params_for(object_age::PARAMS_KEY).cloned(),
            ))),
            other => {
                warn!(
                    policy = other,
                    "unknown MRF policy name in configuration; ignored"
                );
                None
            }
        };
        policies.extend(policy);
    }
    // Pleroma appends `HashtagPolicy` to every chain, whatever is configured.
    policies.push(Box::new(hashtag::HashtagPolicy::new(
        config.params_for(hashtag::PARAMS_KEY),
    )));
    PolicyChain { policies }
}

/// The policy that does nothing (Pleroma's `MRF.NoOpPolicy`): the chain
/// entry that keeps the configuration exercised without touching traffic.
#[derive(Clone, Debug)]
pub struct NoOpPolicy;

impl MrfPolicy for NoOpPolicy {
    fn name(&self) -> &'static str {
        "NoOpPolicy"
    }

    fn filter(&self, _ctx: &MrfContext, activity: Value) -> MrfResult {
        Ok(activity)
    }
}

/// The chain the instance currently runs: [`build_chain`] over the
/// current configuration.
///
/// The instance endpoints describe this chain; the filters build their
/// own so a chain never crosses an await point held by two activities.
#[must_use]
pub async fn active_chain(state: &AppState) -> PolicyChain {
    let config = load_config(state).await;
    build_chain(config.as_ref())
}

/// One pipeline run: the current configuration and the enabled policies'
/// facts preload into the context, and the chain answers for the document.
pub(super) async fn run_chain(state: &AppState, activity: Value) -> MrfDecision {
    let ctx = activity_context(state, &activity).await;
    build_chain(ctx.config.as_ref()).apply(&ctx, activity)
}

/// Reports the policy names; the boxed trait objects are not `Debug`, and
/// their names are the whole interesting part.
impl fmt::Debug for PolicyChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PolicyChain")
            .field("policies", &describe_policies(self))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::*;
    use crate::config::Config;
    use crate::federation::mrf::{
        Transparency, describe_transparency,
        test_support::{
            Rejector, Stamper, config_with, fixed_now, follow_activity, recorded, stamper_chain,
            state_with,
        },
    };

    /// The context the chain tests run: nothing a fake policy reads.
    fn ctx() -> MrfContext {
        MrfContext {
            config: Arc::new(MrfConfig::assemble(Vec::new(), Map::new())),
            now: fixed_now(),
            local_domain: "test.example".to_owned(),
            author_follower_count: None,
            author_status_count: None,
            author_tags: Vec::new(),
            target_tags: Vec::new(),
        }
    }

    /// The chain runs policies in configured order and the first `Err`
    /// short-circuits it: the rejector's reason is the answer and nothing
    /// behind it runs.
    #[test]
    fn the_chain_runs_in_order_and_short_circuits_on_the_first_rejection() {
        let chain = PolicyChain {
            policies: vec![
                Box::new(Stamper {
                    label: "order-a-first",
                }),
                Box::new(Rejector {
                    label: "order-b-reject",
                    reason: "domain is frozen",
                }),
                Box::new(Stamper {
                    label: "order-c-never",
                }),
            ],
        };

        let decision = chain.apply(&ctx(), json!({"touched": []}));

        assert_eq!(
            decision,
            MrfDecision::Reject("domain is frozen".to_owned()),
            "the first Err ends the chain with its reason"
        );
        assert_eq!(
            recorded("order-"),
            vec!["order-a-first", "order-b-reject"],
            "the policy behind the rejector never ran"
        );
    }

    /// Every `Ok` is the document the next policy sees: rewrites
    /// propagate down the chain and the accepted value is the last form.
    #[test]
    fn rewrites_propagate_from_policy_to_policy() {
        let chain = stamper_chain(&["propagate-one", "propagate-two"]);

        let decision = chain.apply(&ctx(), json!({"touched": []}));

        let MrfDecision::Accept(value) = decision else {
            panic!("a chain of stampers accepts");
        };
        assert_eq!(
            value["touched"],
            json!(["propagate-one", "propagate-two"]),
            "the second stamper saw the first one's write"
        );
    }

    /// The empty chain (and the no-op policy) hands the document back
    /// unchanged.
    #[test]
    fn the_empty_chain_accepts_everything_unchanged() {
        let raw = follow_activity();
        let empty = PolicyChain {
            policies: Vec::new(),
        };

        let decision = empty.apply(&ctx(), raw.clone());

        assert_eq!(
            decision,
            MrfDecision::Accept(raw.clone()),
            "the empty chain hands the document back as it came"
        );
        let noop = PolicyChain {
            policies: vec![Box::new(NoOpPolicy)],
        };
        assert_eq!(
            noop.apply(&ctx(), raw),
            decision,
            "and a chain of pure no-ops answers the same"
        );
    }

    /// The no-op policy speaks Pleroma's name and publishes an empty
    /// object under its own key: it has no lists to tell anyone about.
    #[test]
    fn the_noop_policy_is_the_pipeline_placeholder_that_does_nothing() {
        let noop = NoOpPolicy;
        assert_eq!(noop.name(), "NoOpPolicy", "Pleroma's module name");
        assert_eq!(
            Value::Object(noop.describe(&Transparency::open())),
            json!({"mrf_noop": {}}),
            "and no transparency beyond its own key"
        );
        let raw = follow_activity();
        assert_eq!(
            noop.filter(&ctx(), raw.clone()),
            Ok(raw),
            "the filter hands the document onward unchanged"
        );
    }

    /// `build_chain` maps names to constructors in config order, and
    /// unknown names are skipped with a warning: a configuration naming a
    /// policy this server does not have yet accepts everything the way an
    /// empty one does.
    #[test]
    fn build_chain_knows_its_policies_and_skips_unknown_names() {
        let config = config_with(
            &[
                "ghost",
                "noop",
                "simple",
                "anti_link_spam",
                "normalize_markup",
                "tag",
                "keyword",
                "hellthread",
                "object_age",
            ],
            &Value::Null,
        );

        let chain = build_chain(&config);

        assert_eq!(
            describe_policies(&chain),
            vec![
                "NoOpPolicy".to_owned(),
                "SimplePolicy".to_owned(),
                "AntiLinkSpamPolicy".to_owned(),
                "NormalizeMarkupPolicy".to_owned(),
                "TagPolicy".to_owned(),
                "KeywordPolicy".to_owned(),
                "HellthreadPolicy".to_owned(),
                "ObjectAgePolicy".to_owned(),
                "HashtagPolicy".to_owned(),
            ],
            "every known name built its policy in order, the unknown name dropped out \
             and the always-on policy came last"
        );
    }

    /// The hashtag policy is appended to every chain, after the configured
    /// policies, and runs on its default lists when nothing configures it.
    #[test]
    fn build_chain_appends_the_hashtag_policy_last() {
        let chain = build_chain(&config_with(&["noop"], &Value::Null));

        assert_eq!(
            describe_policies(&chain),
            vec!["NoOpPolicy".to_owned(), "HashtagPolicy".to_owned()],
            "the configured policy first, the hashtag policy last"
        );
        assert_eq!(
            describe_policies(&build_chain(&config_with(&[], &Value::Null))),
            vec!["HashtagPolicy".to_owned()],
            "and an empty configuration still runs it"
        );
        assert_eq!(
            describe_transparency(&chain, &Transparency::open())["mrf_hashtag"],
            json!({"sensitive": ["nsfw"], "reject": [], "federated_timeline_removal": []}),
            "with Pleroma's default lists"
        );
    }

    /// The `hashtag` parameter block reaches the policy `build_chain`
    /// appends, and a remote `#nsfw` note leaves the chain sensitive.
    #[test]
    fn the_appended_hashtag_policy_reads_its_params_and_marks_remote_notes() {
        let note = json!({
            "id": "https://peer.example/activities/hashtag-1",
            "type": "Create",
            "actor": "https://peer.example/users/alice",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "object": {
                "id": "https://peer.example/objects/hashtag-1",
                "type": "Note",
                "to": ["https://www.w3.org/ns/activitystreams#Public"],
                "tag": [{"type": "Hashtag", "name": "#NSFW"}],
            },
        });

        let MrfDecision::Accept(accepted) =
            build_chain(&config_with(&[], &Value::Null)).apply(&ctx(), note.clone())
        else {
            panic!("the default lists accept a #nsfw note");
        };
        assert_eq!(
            accepted["object"]["sensitive"],
            json!(true),
            "the remote #nsfw note is stored sensitive"
        );

        let configured = config_with(&[], &json!({"hashtag": {"reject": ["nsfw"]}}));
        assert_eq!(
            build_chain(&configured).apply(&ctx(), note),
            MrfDecision::Reject("[HashtagPolicy] the note carries a rejected hashtag".to_owned()),
            "the configured reject list is the one in force"
        );
    }

    /// The `simple` arm builds its policy from the domain lists the
    /// configuration compiled, so the chain and [`MrfConfig::rejected`]
    /// read one precompilation of one document.
    #[test]
    fn the_simple_policy_gets_the_compiled_lists() {
        let config = config_with(
            &["simple"],
            &json!({"simple": {"reject": ["evil.example"]}}),
        );
        let mut activity = follow_activity();
        let _previous = activity
            .as_object_mut()
            .expect("the fixture is an object")
            .insert(
                "actor".to_owned(),
                json!("https://evil.example/users/alice"),
            );

        let decision = build_chain(&config).apply(&ctx(), activity);

        assert!(
            config.rejected("evil.example"),
            "the configuration refuses the host"
        );
        assert_eq!(
            decision,
            MrfDecision::Reject(
                "[SimplePolicy] the host evil.example is on the `reject` list".to_owned()
            ),
            "and so does the policy built from it"
        );
    }

    /// The actor stage of the chain: a policy that does not implement
    /// [`MrfPolicy::filter_actor`] hands the document back, and the one
    /// that does has its rewrite propagated like any other.
    #[test]
    fn the_actor_stage_passes_a_document_through_every_policy() {
        let person = json!({"id": "https://peer.example/users/alice", "type": "Person"});
        let empty = PolicyChain {
            policies: Vec::new(),
        };
        assert_eq!(
            empty.apply_actor(&ctx(), person.clone()),
            person,
            "the empty chain hands it back"
        );
        let noop = PolicyChain {
            policies: vec![Box::new(NoOpPolicy)],
        };
        assert_eq!(
            noop.apply_actor(&ctx(), person.clone()),
            person,
            "and a policy that says nothing about actors changes nothing"
        );

        let config = config_with(
            &["simple"],
            &json!({"simple": {"avatar_removal": ["peer.example"]}}),
        );
        let mut with_avatar = person.clone();
        let _previous = with_avatar
            .as_object_mut()
            .expect("the fixture is an object")
            .insert("icon".to_owned(), json!("https://peer.example/alice.png"));
        assert_eq!(
            build_chain(&config).apply_actor(&ctx(), with_avatar),
            person,
            "the domain lists strip the avatar the configuration asks for"
        );
    }

    /// `MRF_POLICIES` enables the chain with no database row at all.
    #[tokio::test]
    async fn the_env_variable_enables_policies_without_a_setting() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let mut config = Config::for_tests();
        config.mrf.policies = vec!["noop".to_owned()];
        let state = state_with(db.pool(), config);

        let chain = active_chain(&state).await;

        assert_eq!(
            describe_policies(&chain),
            vec!["NoOpPolicy".to_owned(), "HashtagPolicy".to_owned()],
            "the env list built the chain, the always-on policy behind it"
        );
    }

    /// `build_chain` resolves the three actor-fact policies beside the
    /// domain lists: their Pleroma names come out in configured order, and
    /// each gets the parameter block stored under its own key.
    #[test]
    fn build_chain_resolves_the_actor_fact_policies() {
        let config = config_with(
            &["anti_link_spam", "normalize_markup", "tag"],
            &json!({"tag": {"echo": true}}),
        );

        let chain = build_chain(&config);

        assert_eq!(
            describe_policies(&chain),
            vec![
                "AntiLinkSpamPolicy".to_owned(),
                "NormalizeMarkupPolicy".to_owned(),
                "TagPolicy".to_owned(),
                "HashtagPolicy".to_owned(),
            ],
            "the three names build their policies in order, then the always-on one"
        );
        assert_eq!(
            describe_transparency(&chain, &Transparency::open()),
            Map::from_iter([
                ("mrf_antilinkspam".to_owned(), json!({})),
                ("mrf_normalizemarkup".to_owned(), json!({})),
                ("mrf_tag".to_owned(), json!({"echo": true})),
                (
                    "mrf_hashtag".to_owned(),
                    json!({"sensitive": ["nsfw"], "reject": [], "federated_timeline_removal": []})
                ),
            ]),
            "each policy describes back its own configured subset"
        );
    }

    /// The chain's Debug answers its policy names.
    #[test]
    fn the_chain_debug_names_its_policies() {
        let chain = PolicyChain {
            policies: vec![Box::new(NoOpPolicy)],
        };
        let text = format!("{chain:?}");
        assert!(
            text.contains("NoOpPolicy"),
            "the Debug shows what runs: {text}"
        );
    }
}
