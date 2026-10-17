//! Inbound activity processing: the `federator.incoming` job kind an inbox
//! route enqueues for every accepted `ActivityPub` `POST`, and its handler.
//!
//! The reference is Pleroma's `Pleroma.Web.Federator.incoming_ap_doc/1`
//! (`lib/pleroma/web/federator.ex`), which enqueues the verified params on
//! the `:federator_incoming` queue for `Pleroma.Web.ActivityPub.Transmogrifier`
//! to fold into the database. Here the inbox routes of
//! [`crate::api::activitypub`] authenticate a delivery with the
//! [`SignedActivity`] extractor and enqueue one [`KIND_INCOMING`] job; what
//! the job *does* with the activity — object folding, follow/announce
//! handling, addressing — is the dispatcher ticket's work, not this one's.
//!
//! [`handle_incoming`] is therefore the documented placeholder the ticket
//! prescribes: it keeps the signature the dispatcher will fill in, logs the
//! arrival at debug, and completes. The dispatcher ticket replaces its body
//! (and this module's docs with it); until then the queue drains its
//! incoming jobs as no-ops, exactly as Pleroma's `Transmogrifier`-less
//! development mode does nothing with a received activity.
//!
//! [`SignedActivity`]: crate::federation::inbox_auth::SignedActivity

use serde_json::Value;
use tracing::debug;

use crate::{
    jobs::{AttemptInfo, HandlerFuture, JobError},
    state::AppState,
};

/// The job kind of one received `ActivityPub` activity, on
/// [`QUEUE_FEDERATOR_INCOMING`](crate::jobs::QUEUE_FEDERATOR_INCOMING):
/// Pleroma's `:incoming_ap_doc`, renamed to this project's
/// `federator.incoming` (decision 11).
pub const KIND_INCOMING: &str = "federator.incoming";

/// The [`KIND_INCOMING`] handler — currently a stub, completed by the
/// dispatcher ticket.
///
/// `args` is the object the inbox route wrote:
/// `{"activity": <the received JSON>, "signer_id": <users.id of the
/// authenticated actor>, "inbox_owner": <users.id of the local owner, or
/// null for the shared inboxes>}`. This stub logs the arrival at debug and
/// completes, so the worker marks the job done and the queue drains; the
/// dispatcher ticket replaces the body and keeps the name, the parameters
/// and the `async fn(…) -> Result<(), JobError>` shape (the awaited unit
/// future below is the placeholder its first real `await` replaces).
///
/// # Errors
///
/// Never yet: a stub cannot fail. The dispatcher turns real failures into
/// [`JobError`]s here.
pub async fn handle_incoming(_state: AppState, _args: Value) -> Result<(), JobError> {
    debug!("received an incoming activity; processing lands with the dispatcher ticket");
    async { Ok(()) }.await
}

/// The [`HandlerFn`](crate::jobs::HandlerFn) [`crate::serve`] binds to
/// [`KIND_INCOMING`]: the worker hands it the attempt info the stub does
/// not need yet.
pub fn handle(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move { handle_incoming(state, args).await })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;

    use super::*;
    use crate::{
        config::Config, http_client::HttpClient, jobs::QUEUE_FEDERATOR_INCOMING,
        uploads::UploaderBackend,
    };

    /// Test state over a pool that is never used: the stub touches neither
    /// the database nor the network.
    fn lazy_state() -> AppState {
        let config = Arc::new(Config::for_tests());
        let db = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .unwrap();
        AppState {
            hub: crate::streaming::hub::Hub::new(&db),
            db,
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// The args shape an inbox route writes (see the module docs).
    fn incoming_args() -> Value {
        json!({
            "activity": {
                "@context": "https://www.w3.org/ns/activitystreams",
                "id": "https://peer.example/activities/1",
                "type": "Follow",
                "actor": "https://peer.example/users/alice",
                "object": "https://test.example/users/bob",
            },
            "signer_id": "01940000-0000-7000-8000-000000000001",
            "inbox_owner": null,
        })
    }

    #[test]
    fn the_incoming_kind_is_named_as_the_ticket_fixed() {
        assert_eq!(KIND_INCOMING, "federator.incoming", "the job kind");
    }

    /// The stub does nothing and fails never: the worker's success arm.
    #[tokio::test]
    async fn the_stub_completes() {
        let outcome = handle_incoming(lazy_state(), incoming_args()).await;
        assert_eq!(outcome, Ok(()), "the placeholder completes");
    }

    /// The registry [`crate::serve`] runs knows the kind, and the handler it
    /// binds is this module's stub.
    #[tokio::test]
    async fn the_kind_is_registered_with_this_handler() {
        let registry = crate::job_registry();
        let handler = registry
            .get(KIND_INCOMING)
            .expect("the incoming kind is registered");
        let attempt = AttemptInfo {
            attempts: 1,
            max_attempts: 5,
        };

        let outcome = handler(lazy_state(), attempt, incoming_args()).await;

        assert_eq!(outcome, Ok(()), "the registered stub completes");
        assert_eq!(
            QUEUE_FEDERATOR_INCOMING, "federator_incoming",
            "and the queue the routes enqueue it on is named as fixed"
        );
    }
}
