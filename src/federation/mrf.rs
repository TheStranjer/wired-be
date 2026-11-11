//! The incoming-activity policy pipeline: the hook the dispatcher consults
//! before it routes an activity anywhere.
//!
//! The reference is Pleroma's `Pleroma.Web.ActivityPub.MRF`
//! (`lib/pleroma/web/activity_pub/mrf.ex`): every activity the federator
//! received passes a chain of filters, each of which may rewrite the
//! document or reject it outright. This ticket lays the pipe only:
//! [`filter_incoming`] is the accept-everything implementation the MRF
//! tickets will fill in, and [`MrfDecision`] is the answer every filter
//! stage speaks. The dispatcher ([`super::dispatch`]) drops rejected
//! activities with a debug log and re-normalizes accepted ones from the
//! (possibly rewritten) value the pipeline hands back.

use futures::future::BoxFuture;
use serde_json::Value;

use crate::{federation::dispatch::ActivityView, state::AppState};

/// What the policy pipeline decided about one incoming activity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MrfDecision {
    /// The activity may be dispatched; the value is the document to
    /// dispatch — the original, or the filter's rewritten form of it.
    Accept(Value),
    /// The activity must not be dispatched; the string says why, for the
    /// dispatcher's debug log.
    Reject(String),
}

/// Runs one received activity through the instance's incoming filter
/// chain.
///
/// The placeholder chain is empty: every activity is accepted unchanged,
/// [`MrfDecision::Accept`] carrying the view's own document. The MRF
/// tickets replace this body with the filter pipeline
/// (Pleroma's `MRF.Simple` reject/silence policies and their extensions);
/// the shape of the answer is the contract they build against, so the
/// dispatcher needs no change when they land.
pub fn filter_incoming(state: AppState, view: &ActivityView) -> BoxFuture<'static, MrfDecision> {
    // The placeholder consults neither state nor policy tables; the MRF
    // tickets put both to use. The answer it forms is the document
    // handed back unchanged, so it is read out before the future is.
    let accepted = MrfDecision::Accept(view.raw.clone());
    Box::pin(async move {
        let _state = state;
        accepted
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;
    use std::sync::Arc;

    use super::*;
    use crate::{
        config::Config, federation::dispatch::ObjectRef, http_client::HttpClient,
        uploads::UploaderBackend,
    };

    /// Test state over a pool the placeholder filter never touches.
    fn lazy_state() -> AppState {
        let config = Arc::new(Config::for_tests());
        let db = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no database");
        AppState {
            hub: crate::streaming::hub::Hub::new(&db),
            db,
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// The placeholder accepts every activity unchanged.
    #[tokio::test]
    async fn the_empty_chain_accepts_everything_unchanged() {
        let raw = json!({
            "id": "https://peer.example/activities/1",
            "type": "Follow",
            "actor": "https://peer.example/users/alice",
            "object": "https://test.example/users/bob",
        });
        let view = ActivityView::parse(&raw).expect("the fixture parses");

        let decision = filter_incoming(lazy_state(), &view).await;

        assert_eq!(
            decision,
            MrfDecision::Accept(raw),
            "the accept-everything filter hands the document back as it came"
        );
    }

    /// A rewritten document is what the decision carries, and the two arms
    /// stay distinguishable.
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
