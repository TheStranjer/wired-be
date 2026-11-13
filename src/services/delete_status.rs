//! The status deletion service: the one delete behind every surface that
//! takes a status down.
//!
//! `DELETE /api/v1/statuses/{id}` is the HTTP half of Pleroma's
//! `CommonAPI.delete` (`lib/pleroma/web/common_api.ex`); this module is the
//! write it wraps — the tombstone of the row and of every live reblog of it,
//! the counter steps, and the `after_status_deleted` hook the
//! notification cleanup (and, once those tickets land, the `Delete`
//! activity and the `status_deleted` stream push) hangs off. The endpoint
//! keeps its own gates — authentication, the visibility 404, the ownership
//! rule, the pre-delete render — and calls [`delete_status`] for the write
//! itself.
//!
//! The other callers are the expiration jobs
//! ([`statuses.expire`](crate::jobs::statuses::KIND_EXPIRE) and its
//! [`statuses.expire_sweep`](crate::jobs::statuses::KIND_SWEEP) twin): an
//! expiring post is deleted exactly as its author's delete button deletes
//! it, so they run this same function and nothing else.
use tracing::{debug, warn};

use crate::{
    error::AppResult,
    models::{conversation, notification, status},
    state::AppState,
};

/// Deletes `row` — its tombstone, its reblogs' tombstones, its counters and
/// its post-write hook — exactly as `DELETE /api/v1/statuses/{id}` does.
///
/// The write is [`status::soft_delete_with_reblogs`]: one transaction that
/// tombstones `row` and every live reblog of it and steps the counters each
/// one held up. `true` is the delete that happened, and the
/// `after_status_deleted` hook has run for it by the time this returns;
/// `false` is the row a concurrent delete turned into a tombstone between
/// the caller's gate and this write — the whole operation is idempotent, so
/// an already deleted row moves nothing and fires nothing.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError
pub async fn delete_status(state: &AppState, row: &status::Status) -> AppResult<bool> {
    if status::soft_delete_with_reblogs(&state.db, row.id)
        .await?
        .is_none()
    {
        return Ok(false);
    }
    debug!(
        status = %row.id,
        user = %row.user_id,
        "status deleted"
    );
    after_status_deleted(state, row).await;
    Ok(true)
}

/// Post-write hook of every [`delete_status`]: the notification cleanup that
/// clears the notifications the status caused is wired in here (the
/// tombstoned row never fires the schema's `ON DELETE CASCADE`, so
/// [`notification::delete_for_status`] is what takes the favourites,
/// boosts, reactions and mentions about it with it), and so is the
/// conversation repoint that moves every participation which last saw the
/// deleted status onto its thread's newest remaining direct status — or
/// withdraws the participation when the thread has none (see
/// [`conversation::repoint_after_delete`]). The federation
/// ticket that sends the `Delete` activity (and, with it, the withdrawal
/// of every reblog this delete took down) and the streaming ticket that
/// pushes `status_deleted` to the timelines that carried the post still
/// land here, and the queueing will happen through `state` (decision 11).
///
/// `status` is the row **as it was before the delete** — the delete clears the
/// stored text, so this is the only copy of what the post said: its
/// [`id`](status::Status::id) and [`uri`](status::Status::uri) name the object
/// to withdraw, [`user_id`](status::Status::user_id) is the actor that owns
/// it, and [`reblog_of_id`](status::Status::reblog_of_id) says whether the
/// object was a boost whose original must stop counting it. Pleroma runs all
/// three off the same `CommonAPI.delete` pipeline.
///
/// The cleanup is best-effort: the delete itself is already committed and
/// must not answer a failure the client would retry against a gone
/// status, so a cleanup that fails is logged, not returned.
async fn after_status_deleted(state: &AppState, status: &status::Status) {
    if let Err(err) = notification::delete_for_status(&state.db, status.id).await {
        warn!(
            status = %status.id,
            %err,
            "notification cleanup after status deletion failed"
        );
    }
    if let Err(err) = conversation::repoint_after_delete(&state.db, status.id).await {
        warn!(
            status = %status.id,
            %err,
            "conversation repoint after status deletion failed"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        http_client::HttpClient,
        models::{
            notification::NewNotification,
            status::NewStatus,
            user::{NewUser, User, insert, local_ap_id},
        },
        streaming::hub::Hub,
        uploads::UploaderBackend,
    };

    /// Test state over `pool` with the fixed test config.
    fn app_state(pool: &PgPool) -> AppState {
        let config = Arc::new(Config::for_tests());
        AppState {
            db: pool.clone(),
            hub: Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// Inserts a local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// A public post by `author`.
    async fn post(pool: &PgPool, author: &User, text: &str) -> status::Status {
        status::insert(pool, TEST_BASE_URL, &NewStatus::local(author, text))
            .await
            .expect("the status fixture inserts")
    }

    /// A `favourite` notification of `recipient` by `actor` about `row`.
    async fn favourited(pool: &PgPool, recipient: &User, actor: &User, row: &status::Status) {
        let _fixture = notification::create(
            pool,
            &NewNotification {
                user_id: recipient.id,
                from_account_id: actor.id,
                notification_type: "favourite",
                status_id: Some(row.id),
                emoji: None,
                report_id: None,
                chat_message_id: None,
            },
        )
        .await
        .expect("the notification fixture inserts");
    }

    /// How many notifications name `status_id`.
    async fn notification_count(pool: &PgPool, status_id: uuid::Uuid) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM notifications WHERE status_id = $1")
            .bind(status_id)
            .fetch_one(pool)
            .await
            .expect("the count reads")
    }

    #[tokio::test]
    async fn a_delete_tombstones_the_row_and_clears_its_notifications() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let row = post(&pool, &alice, "doomed").await;
        favourited(&pool, &alice, &bob, &row).await;

        let deleted = delete_status(&app_state(&pool), &row)
            .await
            .expect("the delete runs");

        assert!(deleted, "a live row reports its deletion");
        assert!(
            status::get_by_id(&pool, row.id)
                .await
                .expect("the read runs")
                .is_none(),
            "the row is gone from the live reads"
        );
        let tombstone = status::get_by_id_including_deleted(&pool, row.id)
            .await
            .expect("the read runs")
            .expect("the row is still stored");
        assert!(
            tombstone.deleted_at.is_some(),
            "as a tombstone with its text cleared"
        );
        assert_eq!(tombstone.content, "", "and no content left");
        assert_eq!(
            notification_count(&pool, row.id).await,
            0,
            "the hook took the status's notifications with it"
        );
    }

    #[tokio::test]
    async fn a_second_delete_moves_nothing_and_reports_false() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let row = post(&pool, &alice, "once is enough").await;
        let state = app_state(&pool);
        assert!(
            delete_status(&state, &row)
                .await
                .expect("the first delete runs"),
            "the first pass deletes"
        );
        let tombstone = status::get_by_id_including_deleted(&pool, row.id)
            .await
            .expect("the read runs")
            .expect("the tombstone is stored");

        let again = delete_status(&state, &row)
            .await
            .expect("the second pass runs");

        assert!(!again, "the row was already a tombstone");
        let restated = status::get_by_id_including_deleted(&pool, row.id)
            .await
            .expect("the read runs")
            .expect("the tombstone is still stored");
        assert_eq!(
            restated.updated_at, tombstone.updated_at,
            "the no-op pass did not touch the row"
        );
    }
}
