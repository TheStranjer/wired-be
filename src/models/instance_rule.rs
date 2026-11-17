//! The `instance_rules` table: the ordered rules this instance publishes.
//!
//! Reference: Pleroma's `Pleroma.Rule` (`lib/pleroma/rule.ex`) and its
//! `rules` table — a rule is a piece of `text` plus an explanatory `hint`,
//! sorted by an integer `priority`. Pleroma seeds the list from its
//! configuration through the admin API; here the list only ever lives in
//! the table, and the (later) admin tickets reuse [`create`], [`update`]
//! and [`delete`] to maintain it. `GET /api/v1/instance` and
//! `GET /api/v1/instance/rules` serve [`list`] as the `rules` block.
//!
//! [`list`] orders by `priority` first and the `id` second: id text sorts
//! in creation order (decision 4), so equally ranked rules keep the order
//! they were written in without a separate tie-break column.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

/// [`get_many`]: every rule whose id the array names; an id that names no
/// rule — one deleted after a report cited it — is simply absent from the
/// answer.
const GET_MANY_SQL: &str = "SELECT * FROM instance_rules WHERE id = ANY($1) ORDER BY id";

/// [`create`]: the id this module mints (decision 4) plus the three columns
/// the caller decides on, with `inserted_at` and `updated_at` left to the
/// column defaults. `RETURNING *` brings the row back with those stamps.
const INSERT_SQL: &str = "INSERT INTO instance_rules (id, text, hint, priority) \
     VALUES ($1, $2, $3, $4) RETURNING *";

/// [`update`]: every content column of one rule and the `updated_at` bump,
/// the whole-row write Pleroma's admin form submits. No `WHERE`-clause
/// guard on a missing row: [`update`] reports the miss as `None`.
const UPDATE_SQL: &str = "UPDATE instance_rules
       SET text = $2, hint = $3, priority = $4, updated_at = now()
     WHERE id = $1
   RETURNING *";

/// [`list`]: every rule, lowest `priority` first and, within one priority,
/// oldest first on the id whose text sorts in creation order (decision 4).
const LIST_SQL: &str = "SELECT * FROM instance_rules ORDER BY priority, id";

/// [`delete`]: one rule by id.
const DELETE_SQL: &str = "DELETE FROM instance_rules WHERE id = $1";

/// One row of `instance_rules`: a rule of this instance's list.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct InstanceRule {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4); also the
    /// within-priority order [`list`] reads.
    pub id: Uuid,
    /// The rule as clients display it.
    pub text: String,
    /// Guidance shown beside the rule; empty when the instance gave none.
    pub hint: String,
    /// Sort position: lower shows first.
    pub priority: i32,
    /// When the rule was created.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed; refreshed by every [`update`].
    pub updated_at: DateTime<Utc>,
}

/// Fetches every rule in display order: lowest priority first, rules of one
/// priority oldest first.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list(pool: &PgPool) -> AppResult<Vec<InstanceRule>> {
    let rules = sqlx::query_as::<_, InstanceRule>(LIST_SQL)
        .fetch_all(pool)
        .await?;
    Ok(rules)
}

/// Fetches every rule whose id is in `ids`, in `id` order (creation order,
/// decision 4).
///
/// This is the batched read behind a report entity's `rules` array, which
/// reorders the answer into the reporter's own citation order. Ids that
/// name no rule — one deleted after it was cited — are absent from the
/// answer, and an empty `ids` reads nothing.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_many(pool: &PgPool, ids: &[Uuid]) -> AppResult<Vec<InstanceRule>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rules = sqlx::query_as::<_, InstanceRule>(GET_MANY_SQL)
        .bind(ids)
        .fetch_all(pool)
        .await?;
    Ok(rules)
}

/// Inserts a rule reading `text` with the explanatory `hint` (Pleroma's
/// `hint`, empty for none) at `priority`, and returns the stored row.
///
/// The primary key is minted here (decision 4), so a rule's place among
/// its equally ranked peers is its creation time.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create(
    pool: &PgPool,
    text: &str,
    hint: &str,
    priority: i32,
) -> AppResult<InstanceRule> {
    let row = sqlx::query_as::<_, InstanceRule>(INSERT_SQL)
        .bind(new_id())
        .bind(text)
        .bind(hint)
        .bind(priority)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// Replaces every content column of the rule `id` and returns the
/// rewritten row, `None` when no rule carries that id.
///
/// Like [`create`], a whole-row write: a caller changing one field reads
/// the row, adjusts it and sends the rest back unchanged.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    text: &str,
    hint: &str,
    priority: i32,
) -> AppResult<Option<InstanceRule>> {
    let row = sqlx::query_as::<_, InstanceRule>(UPDATE_SQL)
        .bind(id)
        .bind(text)
        .bind(hint)
        .bind(priority)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Removes the rule `id`, reporting whether there was one to remove.
///
/// # Errors
///
/// Any database failure (connection) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete(pool: &PgPool, id: Uuid) -> AppResult<bool> {
    let outcome = sqlx::query(DELETE_SQL).bind(id).execute(pool).await?;
    Ok(outcome.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ids of `rules`, in list order, as creation-order proof.
    fn ids(rules: &[InstanceRule]) -> Vec<Uuid> {
        rules.iter().map(|rule| rule.id).collect()
    }

    /// [`list`] on a fresh table answers the empty list.
    #[tokio::test]
    async fn list_of_an_empty_table_is_empty() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let rules = list(&pool).await.expect("the list query runs");
        assert!(rules.is_empty(), "a fresh table holds no rules");
    }

    /// [`create`] stores what it is handed, mints the id (decision 4) and
    /// returns the row with its database-stamped timestamps.
    #[tokio::test]
    async fn create_returns_the_stored_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let rule = create(&pool, "Be nice", "say hi first", 3)
            .await
            .expect("the insert runs");
        assert_ne!(rule.id, Uuid::nil(), "the id is minted");
        assert_eq!(rule.text, "Be nice", "text round-trips");
        assert_eq!(rule.hint, "say hi first", "hint round-trips");
        assert_eq!(rule.priority, 3, "priority round-trips");
        assert!(
            rule.updated_at >= rule.inserted_at,
            "both timestamps are stamped"
        );

        let listed = list(&pool).await.expect("the list runs");
        assert_eq!(ids(&listed), vec![rule.id], "the row is listed");
        assert_eq!(listed[0], rule, "list answers the stored row");
    }

    /// The column defaults show through when a caller passes the empties:
    /// [`create`] is the only writer, so the defaults belong to rows that
    /// hand it `""` and `0`.
    #[tokio::test]
    async fn empty_hint_and_zero_priority_store_cleanly() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let rule = create(&pool, "No spam", "", 0)
            .await
            .expect("the insert runs");
        assert_eq!(rule.hint, "", "the empty hint is stored as given");
        assert_eq!(rule.priority, 0, "priority 0 is the default position");
    }

    /// [`list`] orders by priority first and, within one priority, by
    /// creation order on the id (decision 4).
    #[tokio::test]
    async fn list_orders_by_priority_then_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let low = create(&pool, "first rule", "", 1)
            .await
            .expect("first rule inserts");
        let top = create(&pool, "top rule", "read the readme", -5)
            .await
            .expect("top rule inserts");
        let low2 = create(&pool, "second rule", "", 1)
            .await
            .expect("second rule inserts");
        let high = create(&pool, "last rule", "", 10)
            .await
            .expect("last rule inserts");

        let listed = list(&pool).await.expect("the list runs");
        assert_eq!(
            ids(&listed),
            vec![top.id, low.id, low2.id, high.id],
            "priority decides, creation order breaks ties"
        );
    }

    /// [`update`] rewrites every content column of one rule, leaves the
    /// others alone and answers `None` for an id no rule carries.
    #[tokio::test]
    async fn update_rewrites_one_rule_or_misses() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let kept = create(&pool, "keep me", "", 0)
            .await
            .expect("the kept rule inserts");
        let target = create(&pool, "old text", "old hint", 5)
            .await
            .expect("the target rule inserts");

        let rewritten = update(&pool, target.id, "new text", "new hint", 1)
            .await
            .expect("the update runs")
            .expect("the target row exists");
        assert_eq!(rewritten.id, target.id, "the id is not rewritten");
        assert_eq!(rewritten.text, "new text", "text is replaced");
        assert_eq!(rewritten.hint, "new hint", "hint is replaced");
        assert_eq!(rewritten.priority, 1, "priority is replaced");
        assert!(
            rewritten.updated_at > target.updated_at,
            "updated_at restamps"
        );

        let listed = list(&pool).await.expect("the list runs");
        assert_eq!(ids(&listed), vec![kept.id, rewritten.id], "one row each");

        let miss = update(&pool, Uuid::now_v7(), "nobody", "", 0)
            .await
            .expect("the miss still runs");
        assert_eq!(miss, None, "an unknown id updates nothing");
    }

    /// [`get_many`] answers the named rules in id order, skips the ids that
    /// name no rule, and reads nothing for an empty list.
    #[tokio::test]
    async fn get_many_reads_the_named_rules_and_skips_the_gaps() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let first = create(&pool, "first", "", 1)
            .await
            .expect("the first rule inserts");
        let second = create(&pool, "second", "", 2)
            .await
            .expect("the second rule inserts");

        let found = get_many(&pool, &[second.id, first.id])
            .await
            .expect("the batched read runs");
        assert_eq!(
            ids(&found),
            vec![first.id, second.id],
            "id order, not request order"
        );
        assert_eq!(found[1].text, "second", "the rows come back whole");

        let gap = get_many(&pool, &[first.id, Uuid::now_v7()])
            .await
            .expect("an unknown id is no failure");
        assert_eq!(ids(&gap), vec![first.id], "the unknown id is simply absent");

        let _gone = delete(&pool, second.id).await.expect("the rule deletes");
        let after = get_many(&pool, &[first.id, second.id])
            .await
            .expect("the deleted id is no failure either");
        assert_eq!(ids(&after), vec![first.id], "a deleted rule reads as a gap");

        assert!(
            get_many(&pool, &[])
                .await
                .expect("the empty list runs")
                .is_empty(),
            "no ids read nothing"
        );
    }

    /// [`delete`] removes exactly the named rule and reports the second
    /// attempt of the same id as a miss.
    #[tokio::test]
    async fn delete_removes_one_rule() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let doomed = create(&pool, "doomed", "", 0)
            .await
            .expect("the doomed rule inserts");
        let kept = create(&pool, "kept", "", 0)
            .await
            .expect("the kept rule inserts");

        assert!(
            delete(&pool, doomed.id).await.expect("the delete runs"),
            "the first delete hit the row"
        );
        assert!(
            !delete(&pool, doomed.id)
                .await
                .expect("the second delete runs"),
            "deleting twice removes nothing the second time"
        );
        assert_eq!(
            ids(&list(&pool).await.expect("the list runs")),
            vec![kept.id]
        );
    }
}
