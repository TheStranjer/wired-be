//! The capped count of unseen notifications past the caller's marker:
//! [`UnreadParams`] and [`unread_count`], the read behind
//! `GET /api/v2/notifications/unread_count`.

use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::error::AppResult;

use super::flat_page::{NotificationFilters, push_scope};

/// Everything [`unread_count`] needs: whose, filtered how, where the
/// caller's `notifications` marker stands, and the cap.
#[derive(Clone, Debug)]
pub struct UnreadParams<'a> {
    /// The recipient account.
    pub user_id: Uuid,
    /// The client's `types[]` / `exclude_types[]` / `account_id` filters.
    pub filters: NotificationFilters<'a>,
    /// The id the `notifications` marker holds; `None` (no marker, or one
    /// that names no id) counts from the beginning of time.
    pub marker: Option<Uuid>,
    /// The cap on the answer: the count never exceeds it.
    pub limit: i64,
}

/// How many unseen undismissed notifications of `params.user_id` sit past
/// the marker — the `{"count": N}` of `GET /api/v2/notifications/unread_count`.
///
/// The cap is applied inside the count, so a caller asking for at most
/// `limit` gets exactly that when more would match.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn unread_count(pool: &PgPool, params: &UnreadParams<'_>) -> AppResult<i64> {
    let mut qb =
        QueryBuilder::<Postgres>::new("SELECT count(*) FROM (SELECT 1 FROM notifications n");
    push_scope(&mut qb, params.user_id);
    let _unseen: &mut _ = qb.push(" AND NOT n.seen");
    if let Some(marker) = params.marker {
        let _past: &mut _ = qb.push(" AND n.id > ").push_bind(marker);
    }
    params.filters.push(&mut qb);
    let _capped: &mut _ = qb.push(" LIMIT ").push_bind(params.limit).push(") counted");
    let count = qb.build_query_scalar::<i64>().fetch_one(pool).await?;
    Ok(count)
}
