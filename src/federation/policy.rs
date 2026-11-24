//! The instance's domain reject lists, as one question every signed
//! `ActivityPub` request asks of its signer: is this host refused?
//!
//! Both gates that authenticate a peer by HTTP Signature — the inbox
//! extractor ([`crate::federation::inbox_auth`]) and the read gate
//! ([`crate::federation::authorized_fetch`]) — turn a refused host away
//! through `host_refused`, so a rejected instance can neither deliver to
//! this instance nor read from it.

use sqlx::PgPool;
use url::Url;

use crate::{error::AppResult, federation::mrf, models::instance_policy_domain, state::AppState};

/// Whether `host` is on a reject list: the instance-policy table (a row for
/// the host or any parent domain), or the domain lists of the [`mrf`]
/// configuration (its `reject` list, or an `accept` allowlist that does not
/// name it).
///
/// `host` is the canonical spelling [`signer_host`] returns: lowercase,
/// without port. This loads the MRF configuration itself; a caller that
/// already holds one asks [`host_refused_with`].
///
/// # Errors
///
/// Database failures arrive as the usual
/// [`AppError::Database`](crate::error::AppError).
pub(crate) async fn host_refused(state: &AppState, host: &str) -> AppResult<bool> {
    let config = mrf::load_config(state).await;
    host_refused_with(&state.db, config.as_ref(), host).await
}

/// [`host_refused`] over a configuration the caller already loaded: the
/// in-memory MRF lists first, the instance-policy table only when they do
/// not refuse the host.
///
/// # Errors
///
/// Database failures arrive as the usual
/// [`AppError::Database`](crate::error::AppError).
pub(crate) async fn host_refused_with(
    pool: &PgPool,
    config: &mrf::MrfConfig,
    host: &str,
) -> AppResult<bool> {
    if config.rejected(host) {
        return Ok(true);
    }
    instance_policy_domain::is_rejected(pool, host).await
}

/// The host of a signer's key URL, lowercase and without the port.
///
/// It is the authority of its `keyId` (or, equivalently, of the actor URL
/// it points at) — the canonical spelling `instance_policy_domains` rows
/// keep. `None` when the URL cannot be parsed.
#[must_use]
pub fn signer_host(key_id: &str) -> Option<String> {
    let parsed = Url::parse(key_id).ok()?;
    let host = parsed.host_str()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{config::Config, services::runtime_config};

    /// Inserts a policy-table row for `domain`.
    async fn table_row(pool: &PgPool, domain: &str, action: &str) {
        let inserted =
            sqlx::query("INSERT INTO instance_policy_domains (domain, action) VALUES ($1, $2)")
                .bind(domain)
                .bind(action)
                .execute(pool)
                .await
                .expect("the policy row inserts");
        assert_eq!(inserted.rows_affected(), 1, "the row is there");
    }

    /// Stores an MRF `simple` reject list of `patterns`.
    async fn mrf_rejects(pool: &PgPool, patterns: &[&str]) {
        runtime_config::write(
            pool,
            ":pleroma",
            ":mrf",
            &json!({"policies": ["simple"], "simple": {"reject": patterns}}),
        )
        .await
        .expect("the MRF entry is stored");
    }

    /// A host only the table names, a host only the MRF list names, and a
    /// host neither names — through both entry points, and a subdomain of
    /// the table's row.
    #[tokio::test]
    async fn a_host_is_refused_by_the_table_or_the_mrf_list_alone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        table_row(&pool, "table.example", "reject").await;
        table_row(&pool, "quiet.example", "silence").await;
        mrf_rejects(&pool, &["mrf.example"]).await;
        let state = AppState::for_tests(pool.clone(), Config::for_tests());
        let config = mrf::load_config(&state).await;

        for host in ["table.example", "a.table.example", "mrf.example"] {
            assert!(
                host_refused(&state, host).await.unwrap(),
                "`{host}` is refused through the state"
            );
            assert!(
                host_refused_with(&pool, config.as_ref(), host)
                    .await
                    .unwrap(),
                "`{host}` is refused over a preloaded config"
            );
        }
        for host in ["free.example", "quiet.example", "nottable.example"] {
            assert!(
                !host_refused(&state, host).await.unwrap(),
                "`{host}` is on no reject list"
            );
            assert!(
                !host_refused_with(&pool, config.as_ref(), host)
                    .await
                    .unwrap(),
                "`{host}` is on no reject list, preloaded config"
            );
        }
    }

    /// A host the MRF list rejects needs no table row, and a lookup that
    /// cannot reach the database still answers for it.
    #[tokio::test]
    async fn an_mrf_rejection_does_not_consult_the_table() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        mrf_rejects(&pool, &["mrf.example"]).await;
        let state = AppState::for_tests(pool.clone(), Config::for_tests());
        let config = mrf::load_config(&state).await;
        let down = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no database");
        down.close().await;

        assert!(
            host_refused_with(&down, config.as_ref(), "mrf.example")
                .await
                .unwrap(),
            "the in-memory list answers before any query"
        );
        assert!(
            host_refused_with(&down, config.as_ref(), "free.example")
                .await
                .is_err(),
            "a host the lists do not name needs the table, which is down"
        );
    }

    #[test]
    fn signer_host_reads_the_lowercase_authority_host() {
        assert_eq!(
            signer_host("https://Peer.Example/users/alice#main-key").as_deref(),
            Some("peer.example"),
            "host only, portless, case-folded"
        );
        assert_eq!(
            signer_host("http://127.0.0.1:4000/users/alice#main-key").as_deref(),
            Some("127.0.0.1"),
            "the port is not part of the host"
        );
        assert_eq!(
            signer_host("not a url"),
            None,
            "an unparsable keyId names no host"
        );
    }
}
