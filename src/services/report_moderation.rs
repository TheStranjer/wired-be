//! The follow-up every report state change owes, shared by the three admin
//! endpoints that move a report: Pleroma's `PATCH /reports`, Mastodon's
//! `resolve` / `reopen`, and the account action's `report_id`.
//!
//! The assignment write both admin APIs' assign routes share lives here too.

use sqlx::PgPool;
use tracing::debug;
use uuid::Uuid;

use super::webhooks;
use crate::{
    error::AppResult,
    models::{
        moderation_log,
        report::{self, Report as ReportRow, ReportState},
        user::{self, User},
    },
    state::AppState,
};

/// Moves the report `report_id` to `to` and leaves what a state change owes.
///
/// That is its `report_update` moderation-log entry and its `report.updated`
/// event (the webhook fan-out is a queue write, so a subscribed endpoint costs
/// the caller nothing but the insert). Returns the rewritten row, or `None` —
/// with nothing written — when no report carries that id.
///
/// `actor` is named as the moderator who took the action only on a move to
/// [`ReportState::Resolved`]; the other two states decide the action columns
/// on their own ([`report::set_state`] ignores the name for them).
///
/// # Errors
///
/// [`AppError::Database`](crate::error::AppError) from the write, the target
/// read and the log.
pub async fn set_report_state(
    state: &AppState,
    actor: &User,
    report_id: Uuid,
    to: ReportState,
) -> AppResult<Option<ReportRow>> {
    let acted_by = (to == ReportState::Resolved).then_some(actor.id);
    let Some(row) = report::set_state(&state.db, report_id, to, acted_by).await? else {
        return Ok(None);
    };
    let target = user::get_by_id(&state.db, row.target_id).await?;
    let logged = moderation_log::report_update(
        &state.db,
        actor,
        row.id,
        row.state.as_str(),
        target.as_ref(),
    )
    .await?;
    debug!(
        log = %logged.id,
        report = %row.id,
        state = row.state.as_str(),
        "report state changed"
    );
    webhooks::report_updated(state, &row).await;
    Ok(Some(row))
}

/// Hands a report to `assignee`, or back to the unassigned queue on `None`.
///
/// Leaves the entry the move owes: `report_assigned` naming the assignee by
/// nickname, or `report_unassigned`. Assignment is not a state change, so
/// no event fires. Returns the rewritten row, or `None` — with nothing
/// written — when no report carries `report_id`.
///
/// # Errors
///
/// [`AppError::Database`](crate::error::AppError) from the write, the
/// target read and the log.
pub async fn assign_report(
    pool: &PgPool,
    actor: &User,
    report_id: Uuid,
    assignee: Option<&User>,
) -> AppResult<Option<ReportRow>> {
    let Some(row) = report::assign(pool, report_id, assignee.map(|found| found.id)).await? else {
        return Ok(None);
    };
    let target = user::get_by_id(pool, row.target_id).await?;
    let logged = match assignee {
        Some(found) => {
            moderation_log::report_assigned(pool, actor, row.id, &found.nickname, target.as_ref())
                .await?
        }
        None => moderation_log::report_unassigned(pool, actor, row.id, target.as_ref()).await?,
    };
    debug!(
        log = %logged.id,
        report = %row.id,
        assigned = row.assigned_to.is_some(),
        "report assignment changed"
    );
    Ok(Some(row))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        models::{
            report::{Category, NewReport},
            user::{NewUser, insert, local_ap_id},
            webhook,
        },
        services::webhooks::{EVENT_REPORT_UPDATED, KIND_DELIVER},
        util::id::new_id,
    };

    /// A local account named `nickname`.
    async fn local(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                email: Some(format!("{nickname}@example.org")),
                ..NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname))
            },
        )
        .await
        .expect("the local fixture inserts")
    }

    /// The single count `sql` reads, with `value` bound as `$1`.
    async fn count(pool: &PgPool, sql: &'static str, value: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(sql)
            .bind(value)
            .fetch_one(pool)
            .await
            .expect("the count reads")
    }

    /// Resolving writes the row, one `report_update` entry and one delivery
    /// job per subscribed hook, and names the actor as the one who acted.
    #[tokio::test]
    async fn resolving_logs_and_announces_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = AppState::for_tests(pool.clone(), Config::for_tests());
        let actor = local(&pool, "rmactor").await;
        let reporter = local(&pool, "rmreporter").await;
        let target = local(&pool, "rmtarget").await;
        let filed = report::create(
            &pool,
            TEST_BASE_URL,
            &reporter,
            &target,
            &NewReport {
                comment: "spam".to_owned(),
                category: Category::Spam,
                forwarded: false,
                rule_ids: vec![],
                status_ids: vec![],
            },
        )
        .await
        .expect("the report files");
        for url in ["https://example.org/one", "https://example.org/two"] {
            let _hook = webhook::create(&pool, url, &[EVENT_REPORT_UPDATED.to_owned()], true)
                .await
                .expect("the webhook fixture inserts");
        }

        let moved = set_report_state(&state, &actor, filed.id, ReportState::Resolved)
            .await
            .expect("the move runs")
            .expect("the report exists");

        assert_eq!(moved.state, ReportState::Resolved, "the row is resolved");
        assert_eq!(moved.action_taken_by, Some(actor.id), "the actor is named");
        let logged = count(
            &pool,
            "SELECT count(*) FROM moderation_log WHERE action = $1",
            "report_update",
        )
        .await;
        assert_eq!(logged, 1, "one report_update entry");
        let jobs = count(
            &pool,
            "SELECT count(*) FROM jobs WHERE kind = $1",
            KIND_DELIVER,
        )
        .await;
        assert_eq!(jobs, 2, "one delivery job per subscribed hook");
    }

    /// An id that names no report is `Ok(None)` and leaves no entry and no
    /// job behind.
    #[tokio::test]
    async fn an_unknown_report_writes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = AppState::for_tests(pool.clone(), Config::for_tests());
        let actor = local(&pool, "rmnoactor").await;
        let _hook = webhook::create(
            &pool,
            "https://example.org/none",
            &[EVENT_REPORT_UPDATED.to_owned()],
            true,
        )
        .await
        .expect("the webhook fixture inserts");

        let moved = set_report_state(&state, &actor, new_id(), ReportState::Resolved)
            .await
            .expect("the move runs");

        assert!(moved.is_none(), "no report, no row");
        let logged = count(
            &pool,
            "SELECT count(*) FROM moderation_log WHERE action = $1",
            "report_update",
        )
        .await;
        assert_eq!(logged, 0, "no log entry");
        let jobs = count(
            &pool,
            "SELECT count(*) FROM jobs WHERE kind = $1",
            KIND_DELIVER,
        )
        .await;
        assert_eq!(jobs, 0, "no delivery job");
    }

    /// A filed report against a fresh target.
    async fn filed(pool: &PgPool, tag: &str) -> ReportRow {
        let reporter = local(pool, &format!("{tag}reporter")).await;
        let target = local(pool, &format!("{tag}target")).await;
        report::create(
            pool,
            TEST_BASE_URL,
            &reporter,
            &target,
            &NewReport {
                comment: "spam".to_owned(),
                category: Category::Spam,
                forwarded: false,
                rule_ids: vec![],
                status_ids: vec![],
            },
        )
        .await
        .expect("the report files")
    }

    /// Assigning writes the owner and one `report_assigned` entry naming
    /// the assignee; unassigning clears it and writes `report_unassigned`.
    #[tokio::test]
    async fn assigning_and_unassigning_log_once_each() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = local(&pool, "asactor").await;
        let assignee = local(&pool, "asassignee").await;
        let report = filed(&pool, "as").await;

        let moved = assign_report(&pool, &actor, report.id, Some(&assignee))
            .await
            .expect("the assignment runs")
            .expect("the report exists");

        assert_eq!(moved.assigned_to, Some(assignee.id), "the assignee owns it");
        let assign_entries = count(
            &pool,
            "SELECT count(*) FROM moderation_log WHERE action = $1",
            "report_assigned",
        )
        .await;
        assert_eq!(assign_entries, 1, "one report_assigned entry");
        let named = count(
            &pool,
            "SELECT count(*) FROM moderation_log WHERE action = $1 AND message LIKE '%asassignee%'",
            "report_assigned",
        )
        .await;
        assert_eq!(named, 1, "the entry names the assignee");

        let released = assign_report(&pool, &actor, report.id, None)
            .await
            .expect("the release runs")
            .expect("the report exists");

        assert_eq!(released.assigned_to, None, "nobody owns it");
        let unassign_entries = count(
            &pool,
            "SELECT count(*) FROM moderation_log WHERE action = $1",
            "report_unassigned",
        )
        .await;
        assert_eq!(unassign_entries, 1, "one report_unassigned entry");
    }

    /// An id that names no report is `Ok(None)` and leaves no entry.
    #[tokio::test]
    async fn assigning_an_unknown_report_writes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = local(&pool, "asnoactor").await;

        let moved = assign_report(&pool, &actor, new_id(), Some(&actor))
            .await
            .expect("the assignment runs");
        let released = assign_report(&pool, &actor, new_id(), None)
            .await
            .expect("the release runs");

        assert!(moved.is_none(), "no report, no row");
        assert!(released.is_none(), "no report, no row on release either");
        let logged = count(
            &pool,
            "SELECT count(*) FROM moderation_log WHERE action <> $1",
            "report_update",
        )
        .await;
        assert_eq!(logged, 0, "no log entry");
    }
}
