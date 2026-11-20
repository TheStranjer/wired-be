//! The pipeline's configuration: the enabled policy names in chain order
//! beside the parameter document, resolved from the admin config store
//! against the `MRF_POLICIES` fallback and cached for
//! [`MRF_CONFIG_TTL`].

use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Map, Value};
use tracing::warn;

use super::{keyword, simple};
use crate::{services::runtime_config::RuntimeConfig, state::AppState, util::cache::TimedCache};

/// The MRF configuration, resolved from the config store against the
/// `MRF_POLICIES` fallback.
///
/// The enabled policy names in chain order, beside the parameter
/// document the individual policies read their parameters from.
#[derive(Clone, Debug)]
pub struct MrfConfig {
    /// The enabled policies, in the order the chain runs them.
    policies: Vec<String>,
    /// The whole composed document, per [`RuntimeConfig::mrf_document`]:
    /// each policy reads its own key through
    /// [`params_for`](Self::params_for).
    params: Map<String, Value>,
    /// The domain lists of the `simple` policy, compiled once here: the
    /// chain's policy and [`rejected`](Self::rejected) share these rules,
    /// so a match is never computed twice from the same JSON.
    pub(super) simple: Arc<simple::SimpleConfig>,
    /// Whether `simple` is one of the enabled policies: lists that are
    /// configured but switched off govern nothing, here or anywhere.
    simple_enabled: bool,
    /// The keyword patterns of the `keyword` policy, compiled once here
    /// for the same reason: a regex compiles at configuration reads, not
    /// per activity, and an uncompilable one warns once per load.
    pub(super) keyword: Arc<keyword::KeywordConfig>,
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
    /// the domain lists the `simple` key holds and the patterns the
    /// `keyword` key holds compiled beside them.
    pub(super) fn assemble(policies: Vec<String>, params: Map<String, Value>) -> Self {
        let simple_enabled = policies.iter().any(|name| name == simple::PARAMS_KEY);
        let simple = Arc::new(simple::SimpleConfig::from_params(
            params.get(simple::PARAMS_KEY),
        ));
        let keyword = Arc::new(keyword::KeywordConfig::from_params(
            params.get(keyword::PARAMS_KEY),
        ));
        Self {
            policies,
            params,
            simple,
            simple_enabled,
            keyword,
        }
    }

    /// The enabled policy names, in chain order.
    #[must_use]
    pub fn policies(&self) -> &[String] {
        &self.policies
    }

    /// Whether the policy named `name` is one of the enabled ones: the
    /// chain runner asks this before preloading the facts only that
    /// policy reads, so a disabled policy costs no query.
    #[must_use]
    pub fn enabled(&self, name: &str) -> bool {
        self.policies.iter().any(|policy| policy == name)
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

    /// The compiled domain lists of the `simple` policy, `None` while that
    /// policy is not one of the enabled ones: lists that govern no traffic
    /// are published nowhere — the same rule [`rejected`](Self::rejected)
    /// keeps, and the reason `GET /api/v1/instance/domain_blocks` answers
    /// the empty array for an instance that runs no `simple` policy.
    #[must_use]
    pub fn simple(&self) -> Option<&simple::SimpleConfig> {
        self.simple_enabled.then_some(self.simple.as_ref())
    }

    /// Whether the `simple` policy's `reject` list names `host`: the fact
    /// `GET /api/v1/instance/peers` hides those hosts with. Unlike
    /// [`rejected`](Self::rejected) this says nothing about the `accept`
    /// allowlist — a peer the instance has mirrored but never listed is
    /// still a peer; only a host on `reject` is one it will not name.
    #[must_use]
    pub fn on_reject_list(&self, host: &str) -> bool {
        self.simple()
            .is_some_and(|simple| simple.matches(simple::List::Reject, host))
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

/// How long a read of the stored `:mrf*` entries may be served from the
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

/// The instance's current configuration: the cached read while it is
/// fresh, the stored `:mrf*` entries re-read otherwise.
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

/// Reads the config store and caches the answer. A failed read falls back
/// to the env configuration without caching it, so the next activity
/// tries the database again.
async fn reload(state: &AppState, now: DateTime<Utc>) -> Arc<MrfConfig> {
    let store = RuntimeConfig::new(&state.db, &state.config);
    let stored = match store.mrf_document().await {
        Ok(stored) => stored,
        Err(err) => {
            warn!(%err, "the stored `:mrf*` entries could not be read; using the MRF_POLICIES fallback uncached");
            return Arc::new(MrfConfig::resolve(None, &state.config.mrf.policies));
        }
    };
    let config = Arc::new(MrfConfig::resolve(
        stored.as_ref(),
        &state.config.mrf.policies,
    ));
    state.mrf_config_cache.insert(now, Arc::clone(&config));
    config
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::federation::{
        dispatch::ActivityView,
        mrf::{
            MrfDecision, filter_incoming,
            test_support::{
                after, broken_state, config_with, fixed_now, follow_activity, set_mrf_entry,
                test_state,
            },
        },
    };

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

    /// [`MrfConfig::simple`] and [`MrfConfig::on_reject_list`] read the
    /// enabled policy only: lists behind a switched-off `simple` publish
    /// nothing and keep no peer out of the peer list.
    #[test]
    fn the_simple_lists_are_readable_only_while_the_policy_runs() {
        let on = config_with(
            &["simple"],
            &json!({"simple": {"reject": ["*.evil.example"]}}),
        );
        assert!(
            on.simple().is_some(),
            "the running policy's lists are readable"
        );
        assert!(
            on.on_reject_list("cdn.evil.example"),
            "its reject list covers everything below a wildcard"
        );
        assert!(
            !on.on_reject_list("evil.other.example"),
            "and nothing outside it"
        );

        let off = config_with(&["noop"], &json!({"simple": {"reject": ["evil.example"]}}));
        assert!(
            off.simple().is_none(),
            "a disabled policy publishes no lists"
        );
        assert!(
            !off.on_reject_list("evil.example"),
            "and its list hides no peer: the configuration governs nothing"
        );
        let allowlist = config_with(
            &["simple"],
            &json!({"simple": {"accept": ["peer.example"]}}),
        );
        assert!(
            !allowlist.on_reject_list("stranger.example"),
            "an allowlist refuses a host from federating but does not name it \
             on the reject list: the peers listing shows who is mirrored here"
        );
        assert!(
            allowlist.rejected("stranger.example"),
            "while federation does not"
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

    /// The configuration load reads the stored `:mrf` entry, caches it
    /// for [`MRF_CONFIG_TTL`], and re-reads it once the window lapses.
    #[tokio::test]
    async fn the_setting_is_read_once_per_cache_window() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        set_mrf_entry(&pool, ":mrf", &json!({"policies": ["noop"]}))
            .await
            .expect("the setting writes");
        let state = test_state(&pool);

        let first = load_config_at(&state, fixed_now()).await;
        assert_eq!(
            first.policies(),
            ["noop".to_owned()],
            "the row is what loaded"
        );

        set_mrf_entry(&pool, ":mrf", &json!({"policies": []}))
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
}
