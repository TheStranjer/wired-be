//! Fixtures shared by the pipeline modules' unit tests: the fake
//! configuration writes, the chain-building shorthands and the test
//! [`AppState`]s several of them run on.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeDelta, TimeZone as _, Utc};
use serde_json::{Map, Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};

use super::{
    MrfContext, MrfPolicy, MrfResult,
    chain::{PolicyChain, build_chain},
    config::MrfConfig,
};
use crate::{
    config::Config,
    http_client::HttpClient,
    services::{config_registry, runtime_config},
    state::AppState,
    uploads::UploaderBackend,
};

/// Writes a `:mrf*` entry into the admin config store: the source the
/// pipeline's configuration load now reads.
pub(super) async fn set_mrf_entry(
    pool: &PgPool,
    key: &str,
    value: &Value,
) -> crate::error::AppResult<()> {
    runtime_config::write(pool, config_registry::GROUP_PLEROMA, key, value).await
}

/// The configuration of `policies` over the parameter document `params`.
pub(super) fn config_with(policies: &[&str], params: &Value) -> MrfConfig {
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
pub(super) fn fixed_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap()
}

/// The instant `secs` after [`fixed_now`].
pub(super) fn after(secs: i64) -> DateTime<Utc> {
    fixed_now() + TimeDelta::seconds(secs)
}

/// Test state over `pool` and `config`.
pub(super) fn state_with(pool: &PgPool, config: Config) -> AppState {
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
pub(super) fn test_state(pool: &PgPool) -> AppState {
    state_with(pool, Config::for_tests())
}

/// State whose database cannot answer: the failed-read path.
pub(super) async fn broken_state() -> AppState {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://localhost/unused")
        .expect("a lazy pool needs no database");
    pool.close().await;
    test_state(&pool)
}

/// The follow fixture the filters read.
pub(super) fn follow_activity() -> Value {
    json!({
        "id": "https://peer.example/activities/mrf-chain-1",
        "type": "Follow",
        "actor": "https://peer.example/users/alice",
        "object": "https://test.example/users/bob",
    })
}

/// The chain of `lists` under a `simple` policy: the transparency tests
/// all ask what one configured instance of that policy publishes.
pub(super) fn simple_chain(lists: &Value) -> PolicyChain {
    build_chain(&config_with(&["simple"], &json!({"simple": lists})))
}

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
pub(super) fn recorded(prefix: &str) -> Vec<String> {
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
pub(super) struct Stamper {
    /// The label — unique per test, and `name()`'s answer.
    pub(super) label: &'static str,
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
pub(super) struct Rejector {
    /// The label, recorded on the way to rejecting.
    pub(super) label: &'static str,
    /// The rejection reason the chain short-circuits with.
    pub(super) reason: &'static str,
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
pub(super) fn stamper_chain(labels: &[&'static str]) -> PolicyChain {
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
