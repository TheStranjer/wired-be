//! The `instance_policy_domains` table: the domains this instance rejects
//! or silences by instance policy.
//!
//! Reference: Pleroma keeps these in its `:mrf_simple` runtime config and
//! renders `GET /api/v1/instance/domain_blocks` from it
//! (`lib/pleroma/web/mastodon_api/views/instance_view.ex`); here each
//! blocked domain is a row, which is what lets the (later) admin tickets
//! edit the list without rewriting a config blob.
//! [`crate::api::instance`] serves [`list`] as that endpoint, and
//! outbound federation consults [`is_rejected`] before delivering to a
//! host the instance has declared it does not talk to.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::error::AppResult;

/// [`list`]: every policy domain, alphabetically by host, so two reads of
/// an unchanged table answer the same array. The primary key's index
/// already serves this order.
const LIST_SQL: &str = "SELECT * FROM instance_policy_domains ORDER BY domain";

/// [`is_rejected`]: the one fact, asked as an existence check, so a domain
/// that is not on the list costs a `false` and not a scanned table.
const REJECT_EXISTS_SQL: &str = "SELECT EXISTS (SELECT 1 FROM instance_policy_domains \
                                 WHERE domain = $1 AND action = 'reject')";

/// One row of `instance_policy_domains`: a domain the instance's own
/// policy restricts.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct InstancePolicyDomain {
    /// The blocked host, lowercase, no scheme.
    pub domain: String,
    /// What the policy does: `reject`, `silence` or `media_removal`
    /// (the column's `CHECK` keeps it to those three).
    pub action: String,
    /// Why the domain is blocked; empty when the instance gave no reason.
    pub reason: String,
    /// When the row was created.
    pub inserted_at: DateTime<Utc>,
}

/// Fetches every policy domain, alphabetically by host.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list(pool: &PgPool) -> AppResult<Vec<InstancePolicyDomain>> {
    let rows = sqlx::query_as::<_, InstancePolicyDomain>(LIST_SQL)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Whether instance policy rejects `domain` outright: a row for it whose
/// action is `reject`.
///
/// `domain` is compared exactly, so callers pass the canonical spelling the
/// rows hold: lowercase, no scheme, no port (a URL's host already is that —
/// see [`crate::federation::delivery`]). Softer actions (`silence`,
/// `media_removal`) are not rejections: they restrict what is done with a
/// domain's content, not whether this instance talks to it.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn is_rejected(pool: &PgPool, domain: &str) -> AppResult<bool> {
    let rejected = sqlx::query_scalar::<_, bool>(REJECT_EXISTS_SQL)
        .bind(domain)
        .fetch_one(pool)
        .await?;
    Ok(rejected)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inserts one policy row directly: this table has no writer yet
    /// (the admin tickets add one), so the queries here are tested
    /// against hand-written rows.
    async fn insert(pool: &PgPool, domain: &str, action: &str, reason: &str) {
        let inserted = sqlx::query(
            "INSERT INTO instance_policy_domains (domain, action, reason) VALUES ($1, $2, $3)",
        )
        .bind(domain)
        .bind(action)
        .bind(reason)
        .execute(pool)
        .await
        .expect("the policy row inserts");
        assert_eq!(inserted.rows_affected(), 1, "the row is there");
    }

    /// [`list`] on a fresh table answers the empty list.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_of_an_empty_table_is_empty(pool: PgPool) {
        let rows = list(&pool).await.expect("the list query runs");
        assert!(rows.is_empty(), "a fresh table holds no policy");
    }

    /// [`list`] returns every row with its columns intact, alphabetically
    /// by domain.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn list_returns_every_row_by_domain(pool: PgPool) {
        insert(&pool, "spam.example", "reject", "bots").await;
        insert(&pool, "angry.example", "silence", "").await;
        insert(&pool, "lewd.example", "media_removal", "nsfw").await;

        let rows = list(&pool).await.expect("the list query runs");
        assert_eq!(
            rows.iter()
                .map(|row| row.domain.as_str())
                .collect::<Vec<_>>(),
            vec!["angry.example", "lewd.example", "spam.example"],
            "alphabetical by host"
        );
        let reject = rows
            .iter()
            .find(|row| row.domain == "spam.example")
            .expect("the reject row is listed");
        assert_eq!(reject.action, "reject", "action round-trips");
        assert_eq!(reject.reason, "bots", "reason round-trips");
        let silence = rows
            .iter()
            .find(|row| row.domain == "angry.example")
            .expect("the silence row is listed");
        assert_eq!(silence.reason, "", "an ungiven reason is empty");
        assert!(
            silence.inserted_at <= Utc::now(),
            "the row carries its database stamp"
        );
    }

    /// `is_rejected` answers only for a `reject` row on the exact host.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn only_a_reject_row_makes_a_domain_rejected(pool: PgPool) {
        insert(&pool, "spam.example", "reject", "").await;
        insert(&pool, "quiet.example", "silence", "").await;
        insert(&pool, "naked.example", "media_removal", "").await;

        assert!(
            is_rejected(&pool, "spam.example").await.unwrap(),
            "the rejected domain answers true"
        );
        for domain in ["quiet.example", "naked.example"] {
            assert!(
                !is_rejected(&pool, domain).await.unwrap(),
                "`{domain}` is restricted, not rejected"
            );
        }
        for domain in ["SPAM.example", "sub.spam.example", "unknown.example", ""] {
            assert!(
                !is_rejected(&pool, domain).await.unwrap(),
                "`{domain}` names no reject row"
            );
        }
    }

    /// The primary key keeps one policy per domain, and the `action`
    /// `CHECK` refuses anything outside the three documented values.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn the_columns_guard_against_duplicate_and_unknown_rows(pool: PgPool) {
        insert(&pool, "spam.example", "reject", "").await;
        let clash =
            sqlx::query("INSERT INTO instance_policy_domains (domain, action) VALUES ($1, $2)")
                .bind("spam.example")
                .bind("silence")
                .execute(&pool)
                .await;
        assert!(clash.is_err(), "one policy per domain");
        let unknown =
            sqlx::query("INSERT INTO instance_policy_domains (domain, action) VALUES ($1, $2)")
                .bind("other.example")
                .bind("defederate")
                .execute(&pool)
                .await;
        assert!(unknown.is_err(), "action is one of the three");
        let listed = list(&pool).await.expect("the list still runs");
        assert_eq!(listed.len(), 1, "neither rejected row was stored");
    }
}
