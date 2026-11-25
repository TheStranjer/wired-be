//! The quote policy a local author chose: [`QuoteApprovalPolicy`], the
//! `statuses.quote_approval_policy` column that stores it.
//!
//! Mastodon's `POST /api/v1/statuses` and `PUT /api/v1/statuses/:id` take it
//! as `quote_approval_policy: public|followers|nobody`. It only narrows what
//! the post's visibility allows (see
//! [`quote_approval`](crate::federation::quotes::quote_approval)); a remote
//! status advertises its own policy as a [`QuotePolicy`](super::QuotePolicy).

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

/// [`set_quote_approval_policy`]: the live row's policy, replaced (NULL
/// restores the default).
const SET_SQL: &str = "UPDATE statuses
      SET quote_approval_policy = $2
    WHERE id = $1 AND deleted_at IS NULL";

/// Who the author lets quote a local post, stored in the
/// `quote_approval_policy` text column as its lowercase variant name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum QuoteApprovalPolicy {
    /// Anybody who may see the post.
    Public,
    /// The author's accepted followers.
    Followers,
    /// Nobody but the author.
    Nobody,
}

/// The message [`FromStr`] answers for text that names no policy.
const UNKNOWN_POLICY: &str = "unknown quote approval policy";

impl QuoteApprovalPolicy {
    /// The stored (and wire) text of this policy.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Followers => "followers",
            Self::Nobody => "nobody",
        }
    }
}

/// [`as_str`](QuoteApprovalPolicy::as_str) is the text form.
impl fmt::Display for QuoteApprovalPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parses the wire text back, case-sensitively, as clients send it.
impl FromStr for QuoteApprovalPolicy {
    type Err = &'static str;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "public" => Ok(Self::Public),
            "followers" => Ok(Self::Followers),
            "nobody" => Ok(Self::Nobody),
            _ => Err(UNKNOWN_POLICY),
        }
    }
}

/// Replaces the stored policy of the live status `status_id` with `policy`;
/// `None` restores the default.
///
/// A tombstone or a missing row is left alone.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_quote_approval_policy(
    pool: &PgPool,
    status_id: Uuid,
    policy: Option<QuoteApprovalPolicy>,
) -> AppResult<()> {
    sqlx::query(SET_SQL)
        .bind(status_id)
        .bind(policy)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::status::{
        NewStatus, get_by_id, insert,
        test_support::{BASE, post, user},
    };

    #[tokio::test]
    async fn a_policy_is_stored_with_the_row_and_rewritten() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let plain = post(&pool, &alice, "no policy").await;
        assert_eq!(plain.quote_approval_policy, None, "the default is NULL");

        let stored = insert(
            &pool,
            BASE,
            &NewStatus {
                quote_approval_policy: Some(QuoteApprovalPolicy::Followers),
                ..NewStatus::local(&alice, "with policy")
            },
        )
        .await
        .unwrap();
        assert_eq!(
            stored.quote_approval_policy,
            Some(QuoteApprovalPolicy::Followers),
            "the insert stores the author's choice"
        );

        set_quote_approval_policy(&pool, stored.id, Some(QuoteApprovalPolicy::Nobody))
            .await
            .unwrap();
        let row = get_by_id(&pool, stored.id).await.unwrap().unwrap();
        assert_eq!(
            row.quote_approval_policy,
            Some(QuoteApprovalPolicy::Nobody),
            "the policy is rewritten"
        );

        set_quote_approval_policy(&pool, stored.id, None)
            .await
            .unwrap();
        let cleared = get_by_id(&pool, stored.id).await.unwrap().unwrap();
        assert_eq!(
            cleared.quote_approval_policy, None,
            "None restores the default"
        );
    }

    #[test]
    fn the_wire_text_round_trips() {
        for policy in [
            QuoteApprovalPolicy::Public,
            QuoteApprovalPolicy::Followers,
            QuoteApprovalPolicy::Nobody,
        ] {
            assert_eq!(
                policy.as_str().parse::<QuoteApprovalPolicy>(),
                Ok(policy),
                "{policy} reads back"
            );
            assert_eq!(
                policy.to_string(),
                policy.as_str(),
                "the display is the text"
            );
        }
    }

    #[test]
    fn other_text_names_no_policy() {
        assert_eq!(
            "Public".parse::<QuoteApprovalPolicy>(),
            Err(UNKNOWN_POLICY),
            "case matters"
        );
        assert_eq!(
            "".parse::<QuoteApprovalPolicy>(),
            Err(UNKNOWN_POLICY),
            "empty names nothing"
        );
    }

    #[test]
    fn the_policy_serializes_as_its_text() {
        let text = serde_json::to_string(&QuoteApprovalPolicy::Followers).unwrap();
        assert_eq!(text, "\"followers\"", "lowercase, as the column stores it");
        let back: QuoteApprovalPolicy = serde_json::from_str("\"nobody\"").unwrap();
        assert_eq!(back, QuoteApprovalPolicy::Nobody, "and reads back");
    }
}
