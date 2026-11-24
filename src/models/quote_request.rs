//! The `quote_requests` table: the consent a local quote of a remote post
//! asks for (FEP-044f).
//!
//! One row says "the local status `quoting_status_id` quotes the remote
//! status `quoted_status_id`, and we sent its author `quoted_user_id` a
//! `QuoteRequest`". The row starts [`QuoteRequestState::Pending`]; the
//! author's `Accept` makes it [`Accepted`](QuoteRequestState::Accepted) and
//! stores the `QuoteAuthorization` address, their `Reject` makes it
//! [`Rejected`](QuoteRequestState::Rejected), and a later `Delete` of the
//! authorization makes it [`Revoked`](QuoteRequestState::Revoked).
//!
//! Nothing here knows about HTTP or about who may answer: the handlers in
//! [`crate::federation::handlers::quote_answers`] apply that rule.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

/// [`insert`]: write the request, or answer the standing one of the same
/// quoting status.
///
/// The no-op update makes the conflicting row come back through `RETURNING`.
const INSERT_SQL: &str = "INSERT INTO quote_requests
        (id, quoting_status_id, quoted_status_id, quoted_user_id, request_ap_id)
     VALUES ($1, $2, $3, $4, $5)
ON CONFLICT (quoting_status_id) DO UPDATE
        SET quoting_status_id = EXCLUDED.quoting_status_id
  RETURNING *";

/// [`by_request_ap_id`]: one request by the activity id it was sent under.
const BY_REQUEST_SQL: &str = "SELECT * FROM quote_requests WHERE request_ap_id = $1";

/// [`by_authorization_uri`]: one request by the authorization it holds.
const BY_AUTHORIZATION_SQL: &str = "SELECT * FROM quote_requests WHERE authorization_uri = $1";

/// [`accept`]: store the grant.
const ACCEPT_SQL: &str = "UPDATE quote_requests
        SET state = 'accepted', authorization_uri = $2, updated_at = now()
      WHERE id = $1";

/// [`set_state`]: move the request to a state that grants nothing.
const SET_STATE_SQL: &str = "UPDATE quote_requests
        SET state = $2, updated_at = now()
      WHERE id = $1";

/// [`accepted_authorization`]: the address of the grant a status holds.
const ACCEPTED_SQL: &str = "SELECT authorization_uri FROM quote_requests
      WHERE quoting_status_id = $1 AND state = 'accepted'";

/// [`denied_for_statuses`]: the quotes whose author said no, or took the
/// yes back.
const DENIED_SQL: &str = "SELECT quoting_status_id, state FROM quote_requests
      WHERE quoting_status_id = ANY($1) AND state IN ('rejected', 'revoked')";

/// Where one request stands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuoteRequestState {
    /// Sent, not answered yet.
    Pending,
    /// The quoted author granted it.
    Accepted,
    /// The quoted author refused it.
    Rejected,
    /// The quoted author withdrew a grant.
    Revoked,
}

impl QuoteRequestState {
    /// The text the `state` column stores.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Revoked => "revoked",
        }
    }

    /// The state a stored `state` text names; `None` for any other text.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        [Self::Pending, Self::Accepted, Self::Rejected, Self::Revoked]
            .into_iter()
            .find(|state| state.as_str() == text)
    }
}

/// One stored request.
#[derive(Clone, Debug, FromRow)]
pub struct QuoteRequest {
    /// The request's `UUIDv7`, which also names its activity.
    pub id: Uuid,
    /// The local status that quotes.
    pub quoting_status_id: Uuid,
    /// The remote status being quoted.
    pub quoted_status_id: Uuid,
    /// The quoted status' author, the only account whose answer counts.
    pub quoted_user_id: Uuid,
    /// The `QuoteRequest` activity id the request was sent under.
    pub request_ap_id: String,
    /// The stored [`QuoteRequestState`] text.
    pub state: String,
    /// The `QuoteAuthorization` address; `None` until the author grants.
    pub authorization_uri: Option<String>,
    /// When the request was written.
    pub inserted_at: DateTime<Utc>,
    /// When the request last changed.
    pub updated_at: DateTime<Utc>,
}

impl QuoteRequest {
    /// The parsed [`state`](Self::state); an unknown text reads as
    /// [`QuoteRequestState::Pending`], which grants nothing.
    #[must_use]
    pub fn state(&self) -> QuoteRequestState {
        QuoteRequestState::parse(&self.state).unwrap_or(QuoteRequestState::Pending)
    }
}

/// The facts a new request is written from.
#[derive(Clone, Copy, Debug)]
pub struct NewQuoteRequest<'a> {
    /// The local status that quotes.
    pub quoting_status_id: Uuid,
    /// The remote status being quoted.
    pub quoted_status_id: Uuid,
    /// The quoted status' author.
    pub quoted_user_id: Uuid,
    /// The activity id the request will be sent under.
    pub request_uri: &'a str,
}

/// Records the request `new` describes and answers its row.
///
/// Idempotent per quoting status: when a request already stands, that row
/// comes back, with its own `request_ap_id`, so the caller can tell a
/// fresh request from a retry.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn insert(pool: &PgPool, new: &NewQuoteRequest<'_>) -> AppResult<QuoteRequest> {
    let row = sqlx::query_as::<_, QuoteRequest>(INSERT_SQL)
        .bind(new_id())
        .bind(new.quoting_status_id)
        .bind(new.quoted_status_id)
        .bind(new.quoted_user_id)
        .bind(new.request_uri)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// The request sent under the activity id `request_ap_id`, `None` when this
/// instance sent none.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn by_request_ap_id(
    pool: &PgPool,
    request_ap_id: &str,
) -> AppResult<Option<QuoteRequest>> {
    let row = sqlx::query_as::<_, QuoteRequest>(BY_REQUEST_SQL)
        .bind(request_ap_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// The request that holds the authorization `authorization_uri`, `None`
/// when no request does.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn by_authorization_uri(
    pool: &PgPool,
    authorization_uri: &str,
) -> AppResult<Option<QuoteRequest>> {
    let row = sqlx::query_as::<_, QuoteRequest>(BY_AUTHORIZATION_SQL)
        .bind(authorization_uri)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Stores the author's grant: the request `id` becomes accepted and keeps
/// `authorization_uri`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn accept(pool: &PgPool, id: Uuid, authorization_uri: &str) -> AppResult<()> {
    sqlx::query(ACCEPT_SQL)
        .bind(id)
        .bind(authorization_uri)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// Moves the request `id` to `state`, keeping any authorization address
/// it holds so a repeated withdrawal still finds its row.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_state(pool: &PgPool, id: Uuid, state: QuoteRequestState) -> AppResult<()> {
    sqlx::query(SET_STATE_SQL)
        .bind(id)
        .bind(state.as_str())
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The authorization address the quoting status `status_id` holds, `None`
/// unless its request stands accepted.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn accepted_authorization(pool: &PgPool, status_id: Uuid) -> AppResult<Option<String>> {
    let uri = sqlx::query_scalar::<_, Option<String>>(ACCEPTED_SQL)
        .bind(status_id)
        .fetch_optional(pool)
        .await?;
    Ok(uri.flatten())
}

/// Of the quoting statuses `status_ids`, which ones were refused or
/// revoked, and in which state.
///
/// One read for a whole page; an empty list asks the database nothing.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn denied_for_statuses(
    pool: &PgPool,
    status_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, QuoteRequestState>> {
    if status_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(Uuid, String)> = sqlx::query_as(DENIED_SQL)
        .bind(status_ids)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, state)| QuoteRequestState::parse(&state).map(|parsed| (id, parsed)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::status::{self, NewStatus, Status},
        test_fixtures::{local_user, remote_user},
    };

    /// The request id the fixtures send under.
    const REQUEST: &str = "https://test.example/activities/req-1";

    /// A local quote of a remote post, with the request written for it.
    async fn stored(pool: &PgPool) -> (Status, QuoteRequest) {
        let alice = local_user(pool, "alice").await;
        let bob = remote_user(pool, "bob").await;
        let quoted = status::insert(
            pool,
            TEST_BASE_URL,
            &NewStatus {
                uri: Some("https://remote.example/objects/quoted".to_owned()),
                user_id: bob.id,
                ..NewStatus::default()
            },
        )
        .await
        .expect("the quoted post inserts");
        let quoting = status::insert(
            pool,
            TEST_BASE_URL,
            &NewStatus {
                quote_of_id: Some(quoted.id),
                ..NewStatus::local(&alice, "quoting")
            },
        )
        .await
        .expect("the quoting post inserts");
        let row = insert(
            pool,
            &NewQuoteRequest {
                quoting_status_id: quoting.id,
                quoted_status_id: quoted.id,
                quoted_user_id: bob.id,
                request_uri: REQUEST,
            },
        )
        .await
        .expect("the request writes");
        (quoting, row)
    }

    #[test]
    fn the_state_texts_round_trip() {
        for state in [
            QuoteRequestState::Pending,
            QuoteRequestState::Accepted,
            QuoteRequestState::Rejected,
            QuoteRequestState::Revoked,
        ] {
            assert_eq!(
                QuoteRequestState::parse(state.as_str()),
                Some(state),
                "{state:?} reads back"
            );
        }
        assert_eq!(QuoteRequestState::parse("bogus"), None, "unknown text");
    }

    #[tokio::test]
    async fn a_request_starts_pending_and_is_written_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (quoting, row) = stored(pool).await;
        assert_eq!(row.state(), QuoteRequestState::Pending, "nobody answered");
        assert!(row.authorization_uri.is_none(), "nothing is granted");

        let again = insert(
            pool,
            &NewQuoteRequest {
                quoting_status_id: quoting.id,
                quoted_status_id: row.quoted_status_id,
                quoted_user_id: row.quoted_user_id,
                request_uri: "https://test.example/activities/req-2",
            },
        )
        .await
        .expect("the retry writes");

        assert_eq!(again.id, row.id, "the standing row comes back");
        assert_eq!(again.request_ap_id, REQUEST, "with its own request id");
    }

    #[tokio::test]
    async fn a_grant_is_stored_and_found_by_both_ids() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (quoting, row) = stored(pool).await;
        let grant = "https://remote.example/quote_authorizations/1";

        accept(pool, row.id, grant).await.expect("the grant stores");

        assert_eq!(
            accepted_authorization(pool, quoting.id)
                .await
                .expect("the read runs")
                .as_deref(),
            Some(grant),
            "the status holds the grant"
        );
        let by_request = by_request_ap_id(pool, REQUEST)
            .await
            .expect("the read runs")
            .expect("the request is found by its activity id");
        assert_eq!(by_request.state(), QuoteRequestState::Accepted, "accepted");
        let by_grant = by_authorization_uri(pool, grant)
            .await
            .expect("the read runs")
            .expect("the request is found by its grant");
        assert_eq!(by_grant.id, row.id, "the same row");
        assert!(
            by_request_ap_id(pool, "https://remote.example/activities/other")
                .await
                .expect("the read runs")
                .is_none(),
            "an unknown activity id names no request"
        );
    }

    #[tokio::test]
    async fn a_refusal_and_a_revocation_show_as_denied_and_hold_no_grant() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (quoting, row) = stored(pool).await;
        assert!(
            denied_for_statuses(pool, &[quoting.id])
                .await
                .expect("the read runs")
                .is_empty(),
            "a pending request is not denied"
        );
        assert!(
            denied_for_statuses(pool, &[])
                .await
                .expect("the empty read runs")
                .is_empty(),
            "no ids, no answer"
        );

        set_state(pool, row.id, QuoteRequestState::Rejected)
            .await
            .expect("the refusal stores");
        let refused = denied_for_statuses(pool, &[quoting.id])
            .await
            .expect("the read runs");
        assert_eq!(
            refused.get(&quoting.id),
            Some(&QuoteRequestState::Rejected),
            "the refusal is reported"
        );

        accept(
            pool,
            row.id,
            "https://remote.example/quote_authorizations/1",
        )
        .await
        .expect("the grant stores");
        set_state(pool, row.id, QuoteRequestState::Revoked)
            .await
            .expect("the revocation stores");
        let revoked = denied_for_statuses(pool, &[quoting.id])
            .await
            .expect("the read runs");
        assert_eq!(
            revoked.get(&quoting.id),
            Some(&QuoteRequestState::Revoked),
            "the revocation is reported"
        );
        assert!(
            accepted_authorization(pool, quoting.id)
                .await
                .expect("the read runs")
                .is_none(),
            "a revoked request holds no grant"
        );
    }
}
