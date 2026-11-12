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
//! runs. The configuration itself comes from the `mrf` row of the
//! [`instance_settings`](crate::models::instance_setting) table, as JSON
//! of the shape `{"policies": ["simple", …], "simple": {…}, …}`, with the
//! `MRF_POLICIES` environment variable (a comma list) as the fallback for
//! deployments that enable policies without any database row; reads are
//! cached in memory for [`MRF_CONFIG_TTL`], so a busy inbox costs one
//! query per window.
//!
//! The pipeline ships Pleroma's no-op policy
//! (`lib/pleroma/web/activity_pub/mrf/noop_policy.ex`) and its domain lists
//! (`lib/pleroma/web/activity_pub/mrf/simple_policy.ex`, in
//! [`simple`]); further policy tickets add one `match` arm to
//! [`build_chain`] each. Unknown names are warned about and skipped, so a
//! configuration written ahead of a deploy never drops traffic.
//!
//! Actor documents — the profiles this instance mirrors — are not
//! activities, so they run through the chain's other stage,
//! [`filter_actor`], which only the domain-list policy touches and which
//! never rejects: [`upsert_remote_user`](crate::federation::remote_actor::upsert_remote_user)'s
//! callers and the inbound `Update` of an account fold what it answers.
//!
//! The chain's shape is also public metadata: [`describe_policies`] and
//! [`describe_transparency`] feed the `pleroma.metadata.federation` block
//! of the instance endpoints and `NodeInfo`, where Pleroma reports its
//! active policies as `"SimplePolicy"`-style names beside `mrf_simple`
//! transparency objects.

pub mod simple;

use std::{fmt, sync::Arc};

use chrono::{DateTime, TimeDelta, Utc};
use futures::future::BoxFuture;
use serde_json::{Map, Value, json};
use tracing::warn;

use crate::{
    federation::dispatch::ActivityView, models::instance_setting, state::AppState,
    util::cache::TimedCache,
};

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

    /// The policy's public transparency block, served under the
    /// `mrf_<policy>` key of the federation metadata; the default is the
    /// empty object — a policy describes itself once it has lists worth
    /// publishing, and those lists arrive sanitized.
    fn describe(&self) -> Value {
        json!({})
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
}

/// The `mrf` instance setting, resolved against the `MRF_POLICIES`
/// fallback: the enabled policy names in chain order, beside the setting
/// document the individual policies read their parameters from.
#[derive(Clone, Debug)]
pub struct MrfConfig {
    /// The enabled policies, in the order the chain runs them.
    policies: Vec<String>,
    /// The whole setting object as stored; each policy reads its own key
    /// through [`params_for`](Self::params_for).
    params: Map<String, Value>,
    /// The domain lists of the `simple` policy, compiled once here: the
    /// chain's policy and [`rejected`](Self::rejected) share these rules,
    /// so a match is never computed twice from the same JSON.
    simple: Arc<simple::SimpleConfig>,
    /// Whether `simple` is one of the enabled policies: lists that are
    /// configured but switched off govern nothing, here or anywhere.
    simple_enabled: bool,
}

impl MrfConfig {
    /// Resolves one configuration from the stored `mrf` setting and the
    /// `MRF_POLICIES` fallback: the setting's `policies` array wins when
    /// the document carries one, the env list answers otherwise — and
    /// when there is no document at all.
    fn resolve(stored: Option<&Value>, env_policies: &[String]) -> Self {
        let Some(Value::Object(params)) = stored else {
            if stored.is_some() {
                warn!(
                    "the `mrf` instance setting is not a JSON object; falling back to MRF_POLICIES"
                );
            }
            return Self::assemble(env_policies.to_vec(), Map::new());
        };
        let policies = policy_names(params).unwrap_or_else(|| env_policies.to_vec());
        Self::assemble(policies, params.clone())
    }

    /// One resolved chain: the names and the document as configured, with
    /// the domain lists the `simple` key holds compiled beside them.
    fn assemble(policies: Vec<String>, params: Map<String, Value>) -> Self {
        let simple_enabled = policies.iter().any(|name| name == simple::PARAMS_KEY);
        let simple = Arc::new(simple::SimpleConfig::from_params(
            params.get(simple::PARAMS_KEY),
        ));
        Self {
            policies,
            params,
            simple,
            simple_enabled,
        }
    }

    /// The enabled policy names, in chain order.
    #[must_use]
    pub fn policies(&self) -> &[String] {
        &self.policies
    }

    /// The parameter block the policy named `name` was configured with,
    /// `None` when the setting document carries no key for it.
    #[must_use]
    pub fn params_for(&self, name: &str) -> Option<&Value> {
        self.params.get(name)
    }

    /// Whether the instance refuses `host` outright: the `simple` policy's
    /// `reject` list naming it, or its `accept` allowlist being in force
    /// without naming it.
    ///
    /// This is the single source of truth for "rejected domain" beside the
    /// [`instance_policy_domains`](crate::models::instance_policy_domain)
    /// table: the inbox extractor, the delivery enqueue and the publisher
    /// ask it, so an allowlist federation refuses a host everywhere the
    /// table does and not only inside the chain. It answers `false` while
    /// `simple` is not one of the enabled policies, and it knows nothing of
    /// this instance's own domain — every caller asks about a foreign host,
    /// and the chain's own policy adds the local exemption.
    #[must_use]
    pub fn rejected(&self, host: &str) -> bool {
        self.simple_enabled && self.simple.rejected(host)
    }
}

/// The `policies` array of the setting document as name list, `None`
/// (after the log that says why) when the document carries no usable one
/// and the env fallback answers instead.
fn policy_names(params: &Map<String, Value>) -> Option<Vec<String>> {
    let value = params.get("policies")?;
    let Value::Array(names) = value else {
        warn!(
            "the `policies` key of the `mrf` setting is not an array; falling back to MRF_POLICIES"
        );
        return None;
    };
    Some(
        names
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
    )
}

/// How long a read of the `mrf` instance setting may be served from the
/// in-memory cache.
pub const MRF_CONFIG_TTL: TimeDelta = TimeDelta::seconds(30);

/// The [`crate::state::AppState`] slot caching the resolved
/// [`MrfConfig`]; `Default` builds it with [`MRF_CONFIG_TTL`].
pub type MrfConfigCache = TimedCache<Arc<MrfConfig>>;

impl Default for TimedCache<Arc<MrfConfig>> {
    /// An empty config cache with the pipeline's [`MRF_CONFIG_TTL`]
    /// window, so every [`AppState`] constructor writes the same
    /// `Arc::default()`.
    fn default() -> Self {
        Self::new(MRF_CONFIG_TTL)
    }
}

/// The instance setting row the configuration is stored under.
const SETTING_KEY: &str = "mrf";

/// The chain the pipeline runs: enabled policies in configured order.
pub struct PolicyChain {
    /// The policies, in the order [`build_chain`] assembled them.
    policies: Vec<Box<dyn MrfPolicy + Send + Sync>>,
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

/// Reports the chain's policies by their Pleroma-style names, in chain
/// order — the `mrf_policies` list of the federation metadata.
#[must_use]
pub fn describe_policies(chain: &PolicyChain) -> Vec<String> {
    chain
        .policies
        .iter()
        .map(|policy| policy.name().to_owned())
        .collect()
}

/// The chain's transparency blocks keyed for the federation metadata:
/// each policy's [`describe`](MrfPolicy::describe) under its `mrf_<name>`
/// key.
#[must_use]
pub fn describe_transparency(chain: &PolicyChain) -> Map<String, Value> {
    chain
        .policies
        .iter()
        .map(|policy| (transparency_key(policy.name()), policy.describe()))
        .collect()
}

/// The federation-metadata key a policy's transparency block is served
/// under, Pleroma's mapping: `"SimplePolicy"` → `"mrf_simple"`.
fn transparency_key(name: &str) -> String {
    let stem = name.strip_suffix("Policy").unwrap_or(name);
    format!("mrf_{}", stem.to_lowercase())
}

/// Assembles the chain [`MrfConfig`] names.
///
/// Each recognized name gets its policy, each unknown name is warned
/// about and skipped — never a reason to fail an activity. Policy
/// tickets add one `match` arm here each.
#[must_use]
pub fn build_chain(config: &MrfConfig) -> PolicyChain {
    let mut policies: Vec<Box<dyn MrfPolicy + Send + Sync>> = Vec::new();
    for name in config.policies() {
        let policy: Option<Box<dyn MrfPolicy + Send + Sync>> = match name.as_str() {
            "noop" => Some(Box::new(NoOpPolicy)),
            "simple" => Some(Box::new(simple::SimplePolicy::new(Arc::clone(
                &config.simple,
            )))),
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

/// The instance's current configuration: the cached read while it is
/// fresh, the `mrf` instance setting re-read otherwise.
#[must_use]
pub async fn load_config(state: &AppState) -> Arc<MrfConfig> {
    load_config_at(state, Utc::now()).await
}

/// The configuration as of `now`, which is the cache's read instant: the
/// tests roll it to cross the [`MRF_CONFIG_TTL`] boundary without
/// sleeping.
async fn load_config_at(state: &AppState, now: DateTime<Utc>) -> Arc<MrfConfig> {
    match state.mrf_config_cache.get(now) {
        Some(cached) => cached,
        None => reload(state, now).await,
    }
}

/// Reads the setting and caches the answer. A failed read falls back to
/// the env configuration without caching it, so the next activity tries
/// the database again.
async fn reload(state: &AppState, now: DateTime<Utc>) -> Arc<MrfConfig> {
    let stored = match instance_setting::get(&state.db, SETTING_KEY).await {
        Ok(stored) => stored,
        Err(err) => {
            warn!(%err, "the `mrf` instance setting could not be read; using the MRF_POLICIES fallback uncached");
            return Arc::new(MrfConfig::resolve(None, &state.config.mrf_policies));
        }
    };
    let config = Arc::new(MrfConfig::resolve(
        stored.as_ref(),
        &state.config.mrf_policies,
    ));
    state.mrf_config_cache.insert(now, Arc::clone(&config));
    config
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

/// Runs one received activity through the instance's filter chain, over
/// the view's own document: the dispatcher re-normalizes whatever the
/// answer accepts, so a rewrite is what gets routed.
#[must_use]
pub async fn filter_incoming(state: AppState, view: &ActivityView) -> MrfDecision {
    run_chain(&state, view.raw.clone()).await
}

/// [`filter_incoming`] in the shape the dispatcher's `fn`-pointer policy
/// seam takes: the same filter, boxed so it can name no lifetime.
pub fn filter_incoming_boxed(
    state: AppState,
    view: &ActivityView,
) -> BoxFuture<'static, MrfDecision> {
    let raw = view.raw.clone();
    Box::pin(async move { run_chain(&state, raw).await })
}

/// Runs one activity this instance is about to send through the same
/// chain, before any recipient is computed: [`publisher`](super::publisher)
/// delivers the accepted document and nothing at all on a rejection.
#[must_use]
pub async fn filter_outgoing(state: &AppState, activity: Value) -> MrfDecision {
    run_chain(state, activity).await
}

/// [`filter_outgoing`] in the shape of a `fn`-pointer seam, boxed so the
/// caller's borrow cannot leak into the future.
pub fn filter_outgoing_boxed(state: AppState, activity: Value) -> BoxFuture<'static, MrfDecision> {
    Box::pin(async move { run_chain(&state, activity).await })
}

/// Runs one actor document through the instance's chain's actor stage.
///
/// This is what [`upsert_remote_user`](crate::federation::remote_actor::upsert_remote_user)'s
/// callers store and what the inbound `Update` of an account folds, after
/// the policies that strip a profile had their way with it. An actor
/// document cannot be rejected — see
/// [`filter_actor`](MrfPolicy::filter_actor) — so the answer is the
/// document, and a chain that strips nothing hands back exactly what it
/// was given.
#[must_use]
pub async fn filter_actor(state: &AppState, actor: Value) -> Value {
    let ctx = context(state).await;
    build_chain(ctx.config.as_ref()).apply_actor(&ctx, actor)
}

/// The context one pipeline run preloads: the current configuration beside
/// the chain's single shared "now" and this instance's own domain.
async fn context(state: &AppState) -> MrfContext {
    MrfContext {
        config: load_config(state).await,
        now: Utc::now(),
        local_domain: state.config.domain().to_owned(),
    }
}

/// One pipeline run: the current configuration preloads into the context,
/// and the chain answers for the document.
async fn run_chain(state: &AppState, activity: Value) -> MrfDecision {
    let ctx = context(state).await;
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
    use chrono::TimeZone as _;
    use serde_json::Map;
    use sqlx::{PgPool, postgres::PgPoolOptions};
    use std::sync::Mutex;

    use super::*;
    use crate::{
        config::Config, federation::dispatch::ObjectRef, http_client::HttpClient,
        models::instance_setting, uploads::UploaderBackend,
    };

    /// Every `(label, note)` a fake policy ran with, in order. Policies
    /// cannot capture counters through the trait object, so the fakes
    /// record here; tests use labels unique to themselves and never
    /// collide with the suite's concurrent runs.
    static SEEN: Mutex<Vec<(&'static str, &'static str)>> = Mutex::new(Vec::new());

    /// Records a fake policy run.
    fn record(label: &'static str, note: &'static str) {
        SEEN.lock().expect("the test lock").push((label, note));
    }

    /// The labels recorded under `prefix`, in run order.
    fn recorded(prefix: &str) -> Vec<String> {
        SEEN.lock()
            .expect("the test lock")
            .iter()
            .filter(|(label, _)| label.starts_with(prefix))
            .map(|(label, _)| (*label).to_owned())
            .collect()
    }

    /// A fake policy that stamps its `label` into the document's
    /// `touched` array, so ordering and propagation are visible in the
    /// accepted value as well as in [`SEEN`].
    struct Stamper {
        /// The label — unique per test, and `name()`'s answer.
        label: &'static str,
    }

    impl MrfPolicy for Stamper {
        fn name(&self) -> &'static str {
            self.label
        }

        fn filter(&self, _ctx: &MrfContext, mut activity: Value) -> MrfResult {
            record(self.label, "ran");
            activity["touched"]
                .as_array_mut()
                .ok_or_else(|| "fixture activity has a touched array".to_owned())?
                .push(json!(self.label));
            Ok(activity)
        }
    }

    /// A fake policy that rejects with `reason` after recording its run.
    struct Rejector {
        /// The label, recorded on the way to rejecting.
        label: &'static str,
        /// The rejection reason the chain short-circuits with.
        reason: &'static str,
    }

    impl MrfPolicy for Rejector {
        fn name(&self) -> &'static str {
            self.label
        }

        fn filter(&self, _ctx: &MrfContext, _activity: Value) -> MrfResult {
            record(self.label, "ran");
            Err(self.reason.to_owned())
        }
    }

    /// A chain of `stamps` stampers followed by nothing.
    fn stamper_chain(labels: &[&'static str]) -> PolicyChain {
        PolicyChain {
            policies: labels
                .iter()
                .map(|label| {
                    let policy: Box<dyn MrfPolicy + Send + Sync> = Box::new(Stamper { label });
                    policy
                })
                .collect(),
        }
    }

    /// The context the chain tests run: nothing a fake policy reads.
    fn ctx() -> MrfContext {
        MrfContext {
            config: Arc::new(MrfConfig::assemble(Vec::new(), Map::new())),
            now: fixed_now(),
            local_domain: "test.example".to_owned(),
        }
    }

    /// The configuration of `policies` over the parameter document `params`.
    fn config_with(policies: &[&str], params: &Value) -> MrfConfig {
        let mut fields: Map<String, Value> = params.as_object().cloned().unwrap_or_default();
        let _previous = fields.insert(
            "policies".to_owned(),
            Value::Array(policies.iter().map(|name| json!(name)).collect()),
        );
        MrfConfig::assemble(
            policies.iter().map(|name| (*name).to_owned()).collect(),
            fields,
        )
    }

    /// The fixed instant test contexts carry: 2026-11-11T12:00:00Z.
    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap()
    }

    /// The instant `secs` after [`fixed_now`].
    fn after(secs: i64) -> DateTime<Utc> {
        fixed_now() + TimeDelta::seconds(secs)
    }

    /// Test state over `pool` and `config`.
    fn state_with(pool: &PgPool, config: Config) -> AppState {
        let config = Arc::new(config);
        AppState {
            hub: crate::streaming::hub::Hub::new(pool),
            db: pool.clone(),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// Test state on the default test config over `pool`.
    fn test_state(pool: &PgPool) -> AppState {
        state_with(pool, Config::for_tests())
    }

    /// State whose database cannot answer: the failed-read path.
    async fn broken_state() -> AppState {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no database");
        pool.close().await;
        test_state(&pool)
    }

    /// The follow fixture the filters read.
    fn follow_activity() -> Value {
        json!({
            "id": "https://peer.example/activities/mrf-chain-1",
            "type": "Follow",
            "actor": "https://peer.example/users/alice",
            "object": "https://test.example/users/bob",
        })
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

    /// The no-op policy speaks Pleroma's name and describes nothing.
    #[test]
    fn the_noop_policy_is_the_pipeline_placeholder_that_does_nothing() {
        let noop = NoOpPolicy;
        assert_eq!(noop.name(), "NoOpPolicy", "Pleroma's module name");
        assert_eq!(noop.describe(), json!({}), "and no transparency yet");
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
        let config = config_with(&["ghost", "noop", "simple"], &Value::Null);

        let chain = build_chain(&config);

        assert_eq!(
            describe_policies(&chain),
            vec!["NoOpPolicy".to_owned(), "SimplePolicy".to_owned()],
            "`noop` and `simple` built their policies in order and the unknown name dropped out"
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

    /// [`MrfConfig::rejected`] is the allowlist and the reject list read as
    /// one fact: a host the `reject` list names, and every host an active
    /// `accept` list does not name, are refused; the local domain and the
    /// configured peers are not.
    #[test]
    fn rejected_answers_from_the_domain_lists() {
        let reject = config_with(
            &["simple"],
            &json!({"simple": {"reject": ["*.evil.example"]}}),
        );
        assert!(reject.rejected("evil.example"), "the apex of the list");
        assert!(
            reject.rejected("cdn.evil.example"),
            "and everything below it"
        );
        assert!(
            !reject.rejected("friend.example"),
            "a host no list names is free"
        );

        let allowlist = config_with(
            &["simple"],
            &json!({"simple": {"accept": [["peer.example", "our only peer"]]}}),
        );
        assert!(
            !allowlist.rejected("peer.example"),
            "the listed peer federates"
        );
        assert!(
            allowlist.rejected("test.example"),
            "allowlist mode federates with nobody else — not even this instance, \
             whose own exemption is the chain's to add"
        );

        let off = config_with(&["noop"], &json!({"simple": {"reject": ["evil.example"]}}));
        assert!(
            !off.rejected("evil.example"),
            "a configured list behind a disabled policy governs nothing"
        );
        let empty = config_with(&["simple"], &Value::Null);
        assert!(
            !empty.rejected("anyone.example"),
            "and no lists at all refuse nobody"
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

    /// The transparency keys follow Pleroma's mapping, and a chain's
    /// blocks are keyed per policy.
    #[test]
    fn transparency_blocks_key_by_pleroma_convention() {
        assert_eq!(transparency_key("SimplePolicy"), "mrf_simple");
        assert_eq!(transparency_key("NoOpPolicy"), "mrf_noop");
        assert_eq!(transparency_key("ObjectAgePolicy"), "mrf_objectage");
        assert_eq!(transparency_key("Bare"), "mrf_bare");
        let chain = PolicyChain {
            policies: vec![Box::new(NoOpPolicy)],
        };
        assert_eq!(
            describe_transparency(&chain),
            Map::from_iter([("mrf_noop".to_owned(), json!({}))]),
            "the shipped policy's describe() is the empty object"
        );
    }

    /// A stored `policies` array wins over the env fallback, and the
    /// whole setting document stays reachable for the policies.
    #[test]
    fn the_stored_setting_outranks_the_env_fallback() {
        let stored = json!({
            "policies": ["simple", "keyword"],
            "simple": {"reject": ["evil.example"]},
        });
        let env = vec!["noop".to_owned()];

        let config = MrfConfig::resolve(Some(&stored), &env);

        assert_eq!(
            config.policies(),
            ["simple".to_owned(), "keyword".to_owned()],
            "the document's own list is the chain"
        );
        assert_eq!(
            config.params_for("simple"),
            Some(&json!({"reject": ["evil.example"]})),
            "parameters arrive at the policy that owns the key"
        );
        assert_eq!(config.params_for("keyword"), None, "unkeyed policies");
    }

    /// Without a usable `policies` array — no row, no key, a non-object
    /// document, a non-array value — the `MRF_POLICIES` fallback is the
    /// chain.
    #[test]
    fn the_env_fallback_answers_without_stored_policies() {
        let env = vec!["noop".to_owned(), "ghost".to_owned()];
        for stored in [
            None,
            Some(json!({"simple": {}})),
            Some(json!(["policies", "must", "be", "an", "array"])),
            Some(json!({"policies": "not-a-list"})),
        ] {
            let config = MrfConfig::resolve(stored.as_ref(), &env);
            assert_eq!(
                config.policies(),
                env.as_slice(),
                "stored {stored:?} falls back to MRF_POLICIES"
            );
        }
    }

    /// A `policies` array whose elements are not all strings keeps the
    /// strings and drops the rest.
    #[test]
    fn non_string_policy_names_are_dropped() {
        let stored = json!({"policies": ["noop", 7, null, "simple"]});
        let config = MrfConfig::resolve(Some(&stored), &Vec::new());
        assert_eq!(
            config.policies(),
            ["noop".to_owned(), "simple".to_owned()],
            "the string names survive"
        );
    }

    /// The configuration load reads the `mrf` instance setting, caches it
    /// for [`MRF_CONFIG_TTL`], and re-reads it once the window lapses.
    #[tokio::test]
    async fn the_setting_is_read_once_per_cache_window() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        instance_setting::set(&pool, "mrf", &json!({"policies": ["noop"]}))
            .await
            .expect("the setting writes");
        let state = test_state(&pool);

        let first = load_config_at(&state, fixed_now()).await;
        assert_eq!(
            first.policies(),
            ["noop".to_owned()],
            "the row is what loaded"
        );

        instance_setting::set(&pool, "mrf", &json!({"policies": []}))
            .await
            .expect("the rewrite writes");
        let inside = load_config_at(&state, after(29)).await;
        assert_eq!(
            inside.policies(),
            ["noop".to_owned()],
            "one second before expiry the cache still answers"
        );
        assert!(
            Arc::ptr_eq(&first, &inside),
            "and it is literally the same configuration"
        );

        let expired = load_config_at(&state, after(30)).await;
        assert!(
            expired.policies().is_empty(),
            "at the deadline the row is re-read"
        );
    }

    /// A database that cannot answer reads as "no stored setting": the
    /// env fallback runs, uncached, so the next activity tries again.
    #[tokio::test]
    async fn a_failed_read_falls_back_to_the_env_uncached() {
        let state = broken_state().await;
        let activity = follow_activity();
        let view = ActivityView::parse(&activity).expect("the fixture parses");

        let decision = filter_incoming(state.clone(), &view).await;

        assert_eq!(
            decision,
            MrfDecision::Accept(activity),
            "with nothing configured the pipeline accepts unchanged"
        );
        assert!(
            state.mrf_config_cache.get(Utc::now()).is_none(),
            "the fallback answer is not cached: the next read tries the database again"
        );
    }

    /// The incoming filter runs the configured chain over the view's own
    /// document: names resolve, unknown ones vanish, and the document
    /// arrives unchanged.
    #[tokio::test]
    async fn the_incoming_filter_runs_the_configured_chain() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        instance_setting::set(&pool, "mrf", &json!({"policies": ["ghost", "noop"]}))
            .await
            .expect("the setting writes");
        let state = test_state(&pool);
        let activity = follow_activity();
        let view = ActivityView::parse(&activity).expect("the fixture parses");

        let decision = filter_incoming(state, &view).await;

        assert_eq!(
            decision,
            MrfDecision::Accept(activity),
            "noop (beside the skipped ghost name) hands the document back"
        );
    }

    /// The outgoing filter speaks the same answer over the raw document.
    #[tokio::test]
    async fn the_outgoing_filter_accepts_an_unconfigured_chain() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = test_state(db.pool());

        let decision = filter_outgoing(&state, follow_activity()).await;

        assert!(
            matches!(decision, MrfDecision::Accept(_)),
            "no stored row and no env list is the empty chain: {decision:?}"
        );
    }

    /// `MRF_POLICIES` enables the chain with no database row at all.
    #[tokio::test]
    async fn the_env_variable_enables_policies_without_a_setting() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let mut config = Config::for_tests();
        config.mrf_policies = vec!["noop".to_owned()];
        let state = state_with(db.pool(), config);

        let chain = active_chain(&state).await;

        assert_eq!(
            describe_policies(&chain),
            vec!["NoOpPolicy".to_owned()],
            "the env list built the chain"
        );
    }

    /// The context is preloaded for the policies: it carries the local
    /// domain and the shared "now" without the policy touching the
    /// database.
    #[tokio::test]
    async fn the_context_arrives_preloaded() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        instance_setting::set(&pool, "mrf", &json!({"noop": {"probe": true}}))
            .await
            .expect("the setting writes");
        let state = test_state(&pool);

        let config = load_config(&state).await;
        let ctx = MrfContext {
            config,
            now: Utc::now(),
            local_domain: state.config.domain().to_owned(),
        };

        assert_eq!(ctx.local_domain, "test.example", "the preloaded domain");
        assert_eq!(
            ctx.config.params_for("noop"),
            Some(&json!({"probe": true})),
            "and the preloaded setting document"
        );
    }

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
