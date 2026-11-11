//! Instance-wide federation facts the publisher consults but this ticket
//! does not yet store: the per-host reachability ledger and the relay
//! subscriptions.
//!
//! The publisher ([`crate::federation::publisher`]) fans an activity out to
//! remote inboxes, and two of its filters are instance-level lookups the
//! instances and relay tickets own: whether a host is currently marked
//! unreachable, and which relay inboxes want this instance's public and
//! self-broadcast activities. Neither table exists yet, so the two hooks
//! here answer the neutral value — no host is unreachable, no relay is
//! subscribed — and each later ticket replaces only this module's body,
//! leaving every call site in the publisher unchanged.
//!
//! The hooks are plain `fn`s returning a [`BoxFuture`] rather than `async`
//! fns (the shape [`crate::federation::mrf::filter_incoming`] set for
//! exactly this reason): a placeholder that awaits nothing would trip the
//! denied `unused_async` lint, and the call sites' `.await` reads the same
//! either way. They hand back an [`AppResult`] even though the placeholder
//! cannot fail, because the real implementations will query a database,
//! and their callers must not have to change when this module does.

use std::future::ready;

use futures::future::BoxFuture;
use sqlx::PgPool;

use crate::error::AppResult;

/// Whether `host` is marked unreachable for outbound federation.
///
/// The instances ticket (Pleroma's `instances` table with its `unreachable`
/// flag, which `Publisher.publish` consults through
/// `Instances.filter_reachable/1`) fills this in with the real lookup and
/// its failing-then-backing-off bookkeeping. Until then every host is
/// reachable: a publisher blocked on a table that does not exist would
/// deliver nothing at all, so the neutral answer is `Ok(false)`.
pub fn is_host_unreachable<'p>(_pool: &'p PgPool, _host: &str) -> BoxFuture<'p, AppResult<bool>> {
    Box::pin(ready(Ok(false)))
}

/// The inbox URLs of the relays subscribed to this instance.
///
/// The relay ticket (Pleroma's `Pleroma.Web.ActivityPub.Relay` with its
/// `followed_inboxes/0`) fills this in from the stored `Follow`-of-`Relay`
/// subscriptions. Until then no relay follows this instance, so the neutral
/// answer is the empty list.
pub fn relay_inboxes(_pool: &PgPool) -> BoxFuture<'_, AppResult<Vec<String>>> {
    Box::pin(ready(Ok(Vec::new())))
}

#[cfg(test)]
mod tests {
    use sqlx::postgres::PgPoolOptions;

    use super::*;

    /// A lazy pool for placeholder tests that never query.
    fn lazy_pool() -> PgPool {
        PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no database")
    }

    /// Until the instances ticket lands, no host is unreachable.
    #[tokio::test]
    async fn every_host_is_reachable_for_now() {
        let pool = lazy_pool();
        assert!(
            !is_host_unreachable(&pool, "peer.example")
                .await
                .expect("the placeholder never fails"),
            "the hook answers false"
        );
    }

    /// Until the relay ticket lands, nothing is subscribed to relay.
    #[tokio::test]
    async fn nobody_relays_for_now() {
        let pool = lazy_pool();
        assert!(
            relay_inboxes(&pool)
                .await
                .expect("the placeholder never fails")
                .is_empty(),
            "the hook answers the empty list"
        );
    }
}
