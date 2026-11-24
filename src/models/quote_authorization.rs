//! The `quote_authorizations` table: the grants behind FEP-044f quotes.
//!
//! One row says "this local status may be quoted by the remote post at
//! `quoting_uri`, which `quoter_user_id` wrote". Its id names the
//! `QuoteAuthorization` document served at
//! `{BASE_URL}/quote_authorizations/{id}` (see
//! [`crate::federation::quotes`]); a withdrawn grant keeps its row with
//! `revoked_at` set, so the address answers 410 instead of 404.
//!
//! Nothing here knows about HTTP or about who may quote what: the handler
//! that applies the policy lives in
//! [`crate::federation::handlers::quotes`].

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

/// [`grant`]: insert the grant, or restate the quoter's own.
///
/// A restatement keeps the newest request id. A row another account holds
/// for the same post matches no `WHERE` and answers nothing.
const GRANT_SQL: &str = "INSERT INTO quote_authorizations
        (id, quoted_status_id, quoting_uri, quoter_user_id, request_ap_id)
     VALUES ($1, $2, $3, $4, $5)
ON CONFLICT (quoted_status_id, quoting_uri) DO UPDATE
        SET request_ap_id = EXCLUDED.request_ap_id
      WHERE quote_authorizations.quoter_user_id = EXCLUDED.quoter_user_id
  RETURNING *";

/// [`get`]: one grant by id, revoked or not.
const BY_ID_SQL: &str = "SELECT * FROM quote_authorizations WHERE id = $1";

/// [`revoke`]: stamp the withdrawal once; a repeat leaves the first moment.
const REVOKE_SQL: &str = "UPDATE quote_authorizations
        SET revoked_at = now()
      WHERE id = $1 AND revoked_at IS NULL";

/// One stored grant.
#[derive(Clone, Debug, FromRow)]
pub struct QuoteAuthorization {
    /// The grant's `UUIDv7`, which names its document.
    pub id: Uuid,
    /// The local status that may be quoted.
    pub quoted_status_id: Uuid,
    /// The object id of the quoting post, as its instance names it.
    pub quoting_uri: String,
    /// The remote account that asked and wrote the quoting post.
    pub quoter_user_id: Uuid,
    /// The `QuoteRequest` activity that asked, the newest one when the
    /// request was redelivered.
    pub request_ap_id: String,
    /// When the grant was first written.
    pub inserted_at: DateTime<Utc>,
    /// When the grant was withdrawn; `None` while it stands.
    pub revoked_at: Option<DateTime<Utc>>,
}

/// The facts a new grant is written from.
#[derive(Clone, Copy, Debug)]
pub struct NewQuoteAuthorization<'a> {
    /// The local status being quoted.
    pub quoted_status_id: Uuid,
    /// The object id of the quoting post.
    pub quoting_uri: &'a str,
    /// The remote account that asked.
    pub quoter_user_id: Uuid,
    /// The `QuoteRequest` activity id.
    pub request_ap_id: &'a str,
}

/// Records the grant `new` describes and answers its row.
///
/// Idempotent per quoted status and quoting post: a redelivered request
/// answers the standing row (revoked or not) with the newest request id.
/// `None` when the pair is already granted to a different account, which
/// no honest peer produces: the quoting post belongs to one author.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn grant(
    pool: &PgPool,
    new: &NewQuoteAuthorization<'_>,
) -> AppResult<Option<QuoteAuthorization>> {
    let row = sqlx::query_as::<_, QuoteAuthorization>(GRANT_SQL)
        .bind(new_id())
        .bind(new.quoted_status_id)
        .bind(new.quoting_uri)
        .bind(new.quoter_user_id)
        .bind(new.request_ap_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// The grant stored under `id`, `None` when there is none. A revoked grant
/// is returned too, so the route can tell gone from never.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get(pool: &PgPool, id: Uuid) -> AppResult<Option<QuoteAuthorization>> {
    let row = sqlx::query_as::<_, QuoteAuthorization>(BY_ID_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Withdraws the grant `id`; `true` when it was standing.
///
/// The `Delete` of the authorization that tells the quoter's instance is
/// the follow-up this write waits for: nothing federates the withdrawal
/// yet, so the document answers 410 to whoever asks.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn revoke(pool: &PgPool, id: Uuid) -> AppResult<bool> {
    let changed = sqlx::query(REVOKE_SQL)
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(changed > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::status::{self, NewStatus, Status},
        test_fixtures::{local_user, remote_user},
    };

    /// A local post to be quoted.
    async fn quoted_post(pool: &PgPool) -> Status {
        let alice = local_user(pool, "alice").await;
        status::insert(pool, TEST_BASE_URL, &NewStatus::local(&alice, "quote me"))
            .await
            .expect("the quoted post inserts")
    }

    #[tokio::test]
    async fn a_grant_is_stored_and_restated_not_duplicated() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let post = quoted_post(pool).await;
        let bob = remote_user(pool, "bob").await;
        let new = NewQuoteAuthorization {
            quoted_status_id: post.id,
            quoting_uri: "https://remote.example/objects/q1",
            quoter_user_id: bob.id,
            request_ap_id: "https://remote.example/activities/1",
        };

        let first = grant(pool, &new)
            .await
            .expect("the grant writes")
            .expect("a first grant answers its row");
        let again = grant(
            pool,
            &NewQuoteAuthorization {
                request_ap_id: "https://remote.example/activities/2",
                ..new
            },
        )
        .await
        .expect("the restatement writes")
        .expect("the quoter's own grant answers");

        assert_eq!(again.id, first.id, "one grant per post pair");
        assert_eq!(
            again.request_ap_id, "https://remote.example/activities/2",
            "the newest request id is kept"
        );
        assert!(first.revoked_at.is_none(), "a new grant stands");
    }

    #[tokio::test]
    async fn another_accounts_grant_for_the_same_post_is_not_taken_over() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let post = quoted_post(pool).await;
        let bob = remote_user(pool, "bob").await;
        let erin = remote_user(pool, "erin").await;
        let new = NewQuoteAuthorization {
            quoted_status_id: post.id,
            quoting_uri: "https://remote.example/objects/q1",
            quoter_user_id: bob.id,
            request_ap_id: "https://remote.example/activities/1",
        };
        let _first = grant(pool, &new)
            .await
            .expect("the grant writes")
            .expect("bob's grant answers");

        let stolen = grant(
            pool,
            &NewQuoteAuthorization {
                quoter_user_id: erin.id,
                ..new
            },
        )
        .await
        .expect("the competing write runs");

        assert!(
            stolen.is_none(),
            "a grant held by bob is not erin's to restate"
        );
    }

    #[tokio::test]
    async fn a_revoked_grant_stays_readable_and_is_revoked_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let post = quoted_post(pool).await;
        let bob = remote_user(pool, "bob").await;
        let row = grant(
            pool,
            &NewQuoteAuthorization {
                quoted_status_id: post.id,
                quoting_uri: "https://remote.example/objects/q1",
                quoter_user_id: bob.id,
                request_ap_id: "https://remote.example/activities/1",
            },
        )
        .await
        .expect("the grant writes")
        .expect("the grant answers");

        assert!(
            revoke(pool, row.id).await.expect("the revoke runs"),
            "a standing grant is withdrawn"
        );
        assert!(
            !revoke(pool, row.id).await.expect("the repeat runs"),
            "a second withdrawal changes nothing"
        );
        let stored = get(pool, row.id)
            .await
            .expect("the read runs")
            .expect("the revoked row is still there");
        assert!(stored.revoked_at.is_some(), "and says it was withdrawn");
        assert!(
            get(pool, Uuid::now_v7())
                .await
                .expect("the read runs")
                .is_none(),
            "an unknown id is no grant"
        );
    }
}
