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
/// that is not on the list costs a `false` and not a scanned table. `$1` is
/// the host and each of its parent domains ([`host_and_parents`]), matched
/// against the primary key's index.
const REJECT_EXISTS_SQL: &str = "SELECT EXISTS (SELECT 1 FROM instance_policy_domains \
                                 WHERE domain = ANY($1) AND action = 'reject')";

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

/// `host` lowercased, then each parent domain below it, longest first:
/// `A.b.c` → `[a.b.c, b.c, c]`. Matching these against the rows' primary key
/// needs no `LIKE` escaping, and `notevil.example` never yields
/// `evil.example`.
fn host_and_parents(host: &str) -> Vec<String> {
    let host = host.to_ascii_lowercase();
    let mut names = Vec::new();
    let mut rest = host.as_str();
    loop {
        names.push(rest.to_owned());
        match rest.split_once('.') {
            Some((_, parent)) if !parent.is_empty() => rest = parent,
            _ => break,
        }
    }
    names
}

/// Whether instance policy rejects `domain` outright: a `reject` row for it
/// or for any parent domain (a row for `evil.example` covers
/// `a.evil.example`, as Mastodon's `DomainBlock.rule_for` does).
///
/// `domain` is case-folded before matching; callers pass it without scheme
/// or port (a URL's host already is that — see
/// [`crate::federation::delivery`]). Softer actions (`silence`,
/// `media_removal`) are not rejections: they restrict what is done with a
/// domain's content, not whether this instance talks to it.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn is_rejected(pool: &PgPool, domain: &str) -> AppResult<bool> {
    let rejected = sqlx::query_scalar::<_, bool>(REJECT_EXISTS_SQL)
        .bind(host_and_parents(domain))
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
    #[tokio::test]
    async fn list_of_an_empty_table_is_empty() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let rows = list(&pool).await.expect("the list query runs");
        assert!(rows.is_empty(), "a fresh table holds no policy");
    }

    /// [`list`] returns every row with its columns intact, alphabetically
    /// by domain.
    #[tokio::test]
    async fn list_returns_every_row_by_domain() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
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

    /// `host_and_parents` lowercases and walks up to the last label.
    #[test]
    fn host_and_parents_lists_the_host_then_each_parent() {
        assert_eq!(
            host_and_parents("A.b.c"),
            vec!["a.b.c", "b.c", "c"],
            "longest first, case-folded"
        );
        assert_eq!(host_and_parents("solo"), vec!["solo"], "one label");
        assert_eq!(host_and_parents(""), vec![""], "empty stays one empty name");
        assert_eq!(
            host_and_parents("a."),
            vec!["a."],
            "a trailing dot adds no empty parent"
        );
    }

    /// A `reject` row for a domain covers every host beneath it.
    #[tokio::test]
    async fn a_reject_row_covers_its_subdomains() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        insert(&pool, "evil.example", "reject", "").await;
        insert(&pool, "quiet.example", "silence", "").await;

        for host in [
            "evil.example",
            "a.evil.example",
            "x.y.evil.example",
            "A.Evil.Example",
        ] {
            assert!(
                is_rejected(&pool, host).await.unwrap(),
                "`{host}` is covered by the `evil.example` row"
            );
        }
        assert!(
            !is_rejected(&pool, "a.quiet.example").await.unwrap(),
            "a silence row covers no subdomain as a rejection"
        );
    }

    /// A reject row is a domain boundary, not a string suffix.
    #[tokio::test]
    async fn a_reject_row_does_not_cover_a_lookalike_suffix() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        insert(&pool, "evil.example", "reject", "").await;

        for host in [
            "notevil.example",
            "evil.example.org",
            "example",
            "a.notevil.example",
        ] {
            assert!(
                !is_rejected(&pool, host).await.unwrap(),
                "`{host}` is not under `evil.example`"
            );
        }
    }

    /// `is_rejected` answers only for a `reject` row on the host or a parent.
    #[tokio::test]
    async fn only_a_reject_row_makes_a_domain_rejected() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
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
        for domain in ["unknown.example", "example", ""] {
            assert!(
                !is_rejected(&pool, domain).await.unwrap(),
                "`{domain}` names no reject row"
            );
        }
    }

    /// The primary key keeps one policy per domain, and the `action`
    /// `CHECK` refuses anything outside the three documented values.
    #[tokio::test]
    async fn the_columns_guard_against_duplicate_and_unknown_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
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
