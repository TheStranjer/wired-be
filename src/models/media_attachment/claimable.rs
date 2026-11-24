//! The one rule for which uploads a status may claim, and the read built on
//! it.
//!
//! "An upload this user owns that no status holds and no pending schedule
//! reserves" is decided here and nowhere else: [`claimable_predicate`] is the
//! `WHERE` fragment every write that claims media splices in
//! ([`attach_to_status`](super::attach_to_status), the edit's media plan in
//! [`crate::models::status_edit`]) and [`claimable_ids`] is the read the
//! schedule's media gate asks.

use std::collections::HashSet;

use sqlx::{AssertSqlSafe, PgPool};
use uuid::Uuid;

use crate::error::AppResult;

/// The `WHERE` fragment that says a `media_attachments` row aliased `m` is
/// claimable by one user for one status.
///
/// The arguments are SQL expressions (a placeholder such as `$3`, or a
/// literal such as `NULL`), so each query keeps its own bind numbering:
///
/// - `user`: the uploader; remote media has no owner and is never claimable.
/// - `for_status`: a status that may keep rows it already holds (an edit's
///   own media); `NULL` when only unclaimed rows qualify.
/// - `own_schedule`: the pending schedule that is itself the one claiming —
///   its row is still stored while it publishes, so its own reservation must
///   not block it; `NULL` for every other caller.
///
/// A row some status holds is claimable only by that `for_status`. An
/// unclaimed row is claimable unless some pending schedule other than
/// `own_schedule` names it in its stored `media_ids` — the reservation a
/// schedule makes without writing the media row.
#[must_use]
pub fn claimable_predicate(user: &str, for_status: &str, own_schedule: &str) -> String {
    format!(
        "m.user_id = {user}
      AND (m.status_id = ({for_status})::uuid
           OR (m.status_id IS NULL
               AND NOT EXISTS (
                   SELECT 1 FROM scheduled_statuses ss
                    WHERE ss.params -> 'media_ids' @> JSONB_BUILD_ARRAY(m.id::text)
                      AND ss.id IS DISTINCT FROM ({own_schedule})::uuid
               )))"
    )
}

/// Which of `ids` user `user_id` may claim for a new status: their own
/// uploads that no status holds and no pending schedule reserves, in no
/// particular order.
///
/// An unknown id, another account's upload, remote media, a row a status
/// holds and a reserved upload all drop out alike, so callers refusing the
/// request compare the answer with what they asked for. An empty `ids`
/// answers empty without a query.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn claimable_ids(pool: &PgPool, user_id: Uuid, ids: &[Uuid]) -> AppResult<HashSet<Uuid>> {
    if ids.is_empty() {
        return Ok(HashSet::new());
    }
    let sql = format!(
        "SELECT m.id FROM media_attachments m
          WHERE m.id = ANY($1) AND {}",
        claimable_predicate("$2", "NULL", "NULL")
    );
    // The statement is assembled from constants only; every value is bound.
    let found = sqlx::query_scalar::<_, Uuid>(AssertSqlSafe(sql))
        .bind(ids)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(found.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, Utc};
    use serde_json::json;

    use super::super::{
        NewMediaAttachment,
        inserts::insert,
        test_support::{create_status, create_user, new_upload},
    };
    use super::*;
    use crate::{
        models::scheduled_status::{self, NewScheduledStatus, ScheduledStatus},
        util::id::new_id,
    };

    #[tokio::test]
    async fn claimable_ids_keeps_only_unheld_unreserved_own_uploads() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let bob = create_user(&pool, "bob").await;
        let free = insert(&pool, &new_upload(alice.id)).await.unwrap();
        let foreign = insert(&pool, &new_upload(bob.id)).await.unwrap();
        let remote = insert(
            &pool,
            &NewMediaAttachment {
                user_id: None,
                ..new_upload(alice.id)
            },
        )
        .await
        .unwrap();
        let posted = create_status(&pool, &alice).await;
        let held = insert(
            &pool,
            &NewMediaAttachment {
                status_id: Some(posted.id),
                ..new_upload(alice.id)
            },
        )
        .await
        .unwrap();
        let reserved = insert(&pool, &new_upload(alice.id)).await.unwrap();
        let _holder: ScheduledStatus = scheduled_status::insert(
            &pool,
            &NewScheduledStatus {
                user_id: bob.id,
                scheduled_at: Utc::now() + TimeDelta::hours(1),
                params: json!({ "status": "later", "media_ids": [reserved.id.to_string()] }),
                idempotency_key: None,
            },
        )
        .await
        .unwrap()
        .expect("a keyless schedule never conflicts");

        let ids = [
            free.id,
            foreign.id,
            remote.id,
            held.id,
            reserved.id,
            new_id(),
        ];
        let found = claimable_ids(&pool, alice.id, &ids).await.unwrap();

        assert_eq!(
            found,
            HashSet::from([free.id]),
            "only alice's free, unreserved upload is claimable"
        );
        assert!(
            claimable_ids(&pool, alice.id, &[])
                .await
                .unwrap()
                .is_empty(),
            "no ids claim nothing"
        );
    }
}
