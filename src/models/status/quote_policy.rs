//! The quote policy a remote note advertises: [`QuotePolicy`], the
//! `statuses.quote_policy` column that stores it, and [`set_quote_policy`],
//! which rewrites it when the author's `Update` changes it.

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, types::Json};
use uuid::Uuid;

use crate::error::AppResult;

/// [`set_quote_policy`]: the live row's policy, replaced (NULL clears it).
const SET_SQL: &str = "UPDATE statuses
      SET quote_policy = $2
    WHERE id = $1 AND deleted_at IS NULL";

/// Who may quote a remote post, as its `interactionPolicy.canQuote` named
/// them.
///
/// The entries are the approver addresses the document carried, with every
/// spelling of the public address folded to the `as:Public` IRI; mapping
/// them to Mastodon's `public`, `followers` and `following` keys needs the
/// author and happens at render time. A policy with both lists empty means
/// the remote advertised that nobody may quote the post, which is not the
/// same as advertising no policy (the column's NULL).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotePolicy {
    /// Approvers whose quotes are accepted without asking the author
    /// (`automaticApproval`, or the older `always`).
    #[serde(default)]
    pub automatic: Vec<String>,
    /// Approvers whose quotes wait for the author's answer
    /// (`manualApproval`, or the older `approvalRequired`).
    #[serde(default)]
    pub manual: Vec<String>,
}

/// Replaces the stored policy of the live status `status_id` with `policy`;
/// `None` clears it to "advertised nothing".
///
/// A tombstone or a missing row is left alone.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_quote_policy(
    pool: &PgPool,
    status_id: Uuid,
    policy: Option<&QuotePolicy>,
) -> AppResult<()> {
    sqlx::query(SET_SQL)
        .bind(status_id)
        .bind(policy.cloned().map(Json))
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::status::{
        NewStatus, get_by_id,
        test_support::{BASE, post, user},
    };

    /// A policy with one automatic approver.
    fn followers_only() -> QuotePolicy {
        QuotePolicy {
            automatic: vec!["https://remote.example/users/bob/followers".to_owned()],
            manual: Vec::new(),
        }
    }

    #[test]
    fn a_policy_round_trips_through_json() {
        let policy = followers_only();
        let text = serde_json::to_string(&policy).unwrap();
        let back: QuotePolicy = serde_json::from_str(&text).unwrap();
        assert_eq!(back, policy, "the lists survive the column's JSON");
    }

    #[test]
    fn missing_lists_read_as_empty() {
        let policy: QuotePolicy = serde_json::from_str("{}").unwrap();
        assert_eq!(policy, QuotePolicy::default(), "both lists default empty");
    }

    #[tokio::test]
    async fn a_policy_is_stored_with_the_row_and_rewritten() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let plain = post(&pool, &alice, "no policy").await;
        assert_eq!(plain.quote_policy, None, "a local post stores no policy");

        let stored = crate::models::status::insert(
            &pool,
            BASE,
            &NewStatus {
                quote_policy: Some(followers_only()),
                ..NewStatus::local(&alice, "with policy")
            },
        )
        .await
        .unwrap();
        assert_eq!(
            stored.quote_policy,
            Some(Json(followers_only())),
            "the insert returns the stored policy"
        );

        set_quote_policy(&pool, stored.id, Some(&QuotePolicy::default()))
            .await
            .unwrap();
        let row = get_by_id(&pool, stored.id).await.unwrap().unwrap();
        assert_eq!(
            row.quote_policy,
            Some(Json(QuotePolicy::default())),
            "an empty policy is stored, not cleared"
        );

        set_quote_policy(&pool, stored.id, None).await.unwrap();
        let cleared = get_by_id(&pool, stored.id).await.unwrap().unwrap();
        assert_eq!(cleared.quote_policy, None, "None clears the column");
    }
}
