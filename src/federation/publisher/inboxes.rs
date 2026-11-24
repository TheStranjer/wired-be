//! The last stage of fan-out: reducing the candidate inboxes to the ones a
//! delivery may actually go to — deduplicated by URL, `http(s)`, on a
//! foreign authority, on a host neither instance policy rejects nor the
//! unreachable ledger marks past the grace window.

use std::collections::HashSet;

use sqlx::PgPool;
use tracing::debug;

use crate::{
    error::AppResult,
    federation::{delivery::is_local_inbox, mrf},
    models::{instance, instance_policy_domain, user::User},
    util::url::parse_http_url,
};

/// The final target set: candidates deduplicated by URL, each kept only
/// when it is an `http(s)` inbox on a foreign authority whose host neither
/// instance policy rejects — by table row nor by the [`mrf`] domain lists
/// ([`MrfConfig::rejected`]) — nor the unreachable ledger marks past `grace`.
///
/// [`mrf`]: crate::federation::mrf
/// [`MrfConfig::rejected`]: crate::federation::mrf::MrfConfig::rejected
pub(super) async fn deliverable_inboxes(
    pool: &PgPool,
    policy: &mrf::MrfConfig,
    actor: &User,
    candidates: Vec<String>,
    grace: chrono::Duration,
) -> AppResult<Vec<String>> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut inboxes = Vec::new();
    for candidate in candidates {
        if !seen.insert(candidate.clone()) {
            continue;
        }
        let Some(host) = deliverable_host(&candidate, &actor.ap_id) else {
            continue;
        };
        if instance_policy_domain::is_rejected(pool, &host).await?
            || policy.rejected(&host)
            || instance::is_host_unreachable(pool, &host, grace).await?
        {
            debug!(inbox = %candidate, "delivery target dropped: the host is refused");
            continue;
        }
        inboxes.push(candidate);
    }
    Ok(inboxes)
}

/// The configured [`UNREACHABLE_GRACE_SECS`](crate::config::UNREACHABLE_GRACE_SECS_VAR)
/// as the span [`is_host_unreachable`](crate::models::instance::is_host_unreachable)
/// compares a mark against: `secs` seconds, and [`Duration::MAX`](chrono::Duration::MAX)
/// for a value too large to represent as a span — the literal reading of a
/// window that outlasts every mark is a gate that never closes, which is what
/// such a value asks for.
///
/// The one conversion from the setting's `usize` seconds to the interval the
/// lookup binds, done once per publish so the per-inbox loop passes a span.
pub(super) fn unreachable_grace(secs: usize) -> chrono::Duration {
    chrono::Duration::try_seconds(i64::try_from(secs).unwrap_or(i64::MAX))
        .unwrap_or(chrono::Duration::MAX)
}

/// The host of `inbox`, when it is a deliverable `http(s)` URL on an
/// authority other than the publishing actor's own — an inbox on this
/// instance's origin holds local recipients, whom a delivery would only
/// ask this process to `POST` to itself.
fn deliverable_host(inbox: &str, actor_ap_id: &str) -> Option<String> {
    parse_http_url(inbox)
        .filter(|parsed| !is_local_inbox(parsed, actor_ap_id))
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only foreign `http(s)` inboxes are deliverable targets; this
    /// instance's own origin and unparseable URLs are not.
    #[test]
    fn only_foreign_http_inboxes_are_deliverable() {
        let alice = "https://test.example/users/alice";
        assert_eq!(
            deliverable_host("https://peer.example/inbox", alice).as_deref(),
            Some("peer.example"),
            "a foreign https inbox"
        );
        assert_eq!(
            deliverable_host("http://peer.example:8080/inbox", alice).as_deref(),
            Some("peer.example"),
            "a port does not change the host"
        );
        assert_eq!(
            deliverable_host("https://test.example.evil.example/inbox", alice).as_deref(),
            Some("test.example.evil.example"),
            "a look-alike prefix host is foreign"
        );
        for own in [
            "https://test.example/users/bob/inbox",
            "https://test.example:443/inbox",
        ] {
            assert_eq!(
                deliverable_host(own, alice),
                None,
                "`{own}` is this instance's own origin"
            );
        }
        for undeliverable in ["ftp://peer.example/inbox", "not a url", ""] {
            assert_eq!(
                deliverable_host(undeliverable, alice),
                None,
                "`{undeliverable}` is no deliverable inbox"
            );
        }
    }
}
