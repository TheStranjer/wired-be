//! The event hooks: the report and account payload builders that feed
//! [`emit`].

use serde_json::{Value, json};
use tracing::warn;

use crate::{
    entities::{admin_account::AdminAccount, report::Report},
    error::{AppError, AppResult},
    models::{
        report::{self, Report as ReportRow},
        user::{self, User},
    },
    state::AppState,
};

use super::{EVENT_ACCOUNT_CREATED, EVENT_REPORT_CREATED, EVENT_REPORT_UPDATED, emit};

/// The `report.created` hook: the filed `report` as `{"report": …}`.
///
/// The payload is the Mastodon [`Report`] render — this codebase has no admin
/// report entity yet, so the one report shape there is goes out — with the
/// target account seen as an anonymous reader sees it: the event says what was
/// filed, not what any particular viewer may read.
pub async fn report_created(state: &AppState, report: &ReportRow) {
    emit_report_event(state, EVENT_REPORT_CREATED, report).await;
}

/// The `report.updated` hook: the report moderation moved, in the same
/// envelope as [`report_created`].
pub async fn report_updated(state: &AppState, report: &ReportRow) {
    emit_report_event(state, EVENT_REPORT_UPDATED, report).await;
}

/// The two report events' shared half: build the payload, then [`emit`]. A
/// report that cannot be rendered is logged and skipped — the filing or the
/// state change it would have announced has already been committed.
async fn emit_report_event(state: &AppState, event: &str, row: &ReportRow) {
    match report_payload(state, row).await {
        Ok(data) => emit(state, event, data).await,
        Err(err) => {
            warn!(%err, report = %row.id, event, "the webhook event could not be rendered");
        }
    }
}

/// `{"report": …}`: the row as the [`Report`] entity, with the cited statuses
/// read back off `report_statuses` and the target account behind
/// [`target_id`](ReportRow::target_id).
///
/// # Errors
///
/// [`AppError::NotFound`] when the reported account is already gone (an
/// deleting cascade got there first, so there is nothing to describe),
/// [`AppError::Database`] from either read and [`AppError::Internal`] for an
/// entity that somehow cannot serialize.
async fn report_payload(state: &AppState, row: &ReportRow) -> AppResult<Value> {
    let target = user::get_by_id(&state.db, row.target_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let cited = report::statuses_for(&state.db, row.id).await?;
    let entity = Report::from_row(row, &target, &cited, &state.config, None);
    serde_json::to_value(entity)
        .map(|report| json!({"report": report}))
        .map_err(|err| AppError::Internal(format!("the report entity did not serialize: {err}")))
}

/// The `account.created` hook: the freshly registered local `account` as
/// `{"account": …}`.
///
/// The render is [`AdminAccount`]'s — the shape Pleroma's `account.created`
/// delivery carries, since the event is for staff tooling and it names the
/// address and the flags registration set.
pub async fn account_created(state: &AppState, account: &User) {
    match account_payload(state, account) {
        Ok(data) => emit(state, EVENT_ACCOUNT_CREATED, data).await,
        Err(err) => {
            warn!(%err, account = %account.id, "the account.created event could not be rendered");
        }
    }
}

/// `{"account": …}`: the row as the admin account entity.
///
/// # Errors
///
/// [`AppError::Internal`] for an entity that somehow cannot serialize.
fn account_payload(state: &AppState, account: &User) -> AppResult<Value> {
    let entity = AdminAccount::from_user(account, &state.config);
    serde_json::to_value(entity)
        .map(|rendered| json!({"account": rendered}))
        .map_err(|err| AppError::Internal(format!("the account entity did not serialize: {err}")))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::super::test_support::{
        app_state, dead_state, deliveries, filed, fresh_id, hook, local,
    };
    use super::*;
    use crate::models::report::Category;

    #[tokio::test]
    async fn the_report_hooks_announce_the_filed_report() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let reporter = local(&pool, "whreporter").await;
        let target = local(&pool, "whtarget").await;
        let row = filed(&pool, &reporter, &target).await;
        let _created = hook(&pool, "https://example.org/a", &[EVENT_REPORT_CREATED]).await;
        let _updated = hook(&pool, "https://example.org/b", &[EVENT_REPORT_UPDATED]).await;

        report_created(&state, &row).await;
        report_updated(&state, &row).await;

        let queued = deliveries(&pool).await;
        assert_eq!(queued.len(), 2, "one job per event, one event per hook");
        let events: Vec<&str> = queued
            .iter()
            .map(|job| job.args["event"].as_str().expect("the job names its event"))
            .collect();
        assert!(
            events.contains(&EVENT_REPORT_CREATED) && events.contains(&EVENT_REPORT_UPDATED),
            "both report events fired: {events:?}"
        );
        for job in &queued {
            let data = &job.args["data"];
            assert_eq!(data["report"]["id"], row.id.to_string(), "the report id");
            assert_eq!(data["report"]["comment"], row.comment, "the comment");
            assert_eq!(data["report"]["category"], "spam", "the category");
            assert_eq!(
                data["report"]["target_account"]["acct"], target.nickname,
                "the reported account"
            );
            assert!(
                data["report"].get("action_taken").is_some(),
                "the entity's moderation flag rides along"
            );
        }
    }

    #[tokio::test]
    async fn a_report_that_cannot_be_described_queues_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let reporter = local(&pool, "gone-reporter").await;
        let target = local(&pool, "gone-target").await;
        let row = filed(&pool, &reporter, &target).await;
        let _hook = hook(&pool, "https://example.org/gone", &[EVENT_REPORT_UPDATED]).await;
        // A row whose target account the deletion cascade already took.
        let orphan = ReportRow {
            target_id: fresh_id(),
            ..row.clone()
        };

        report_updated(&state, &orphan).await;

        assert!(
            deliveries(&pool).await.is_empty(),
            "an unrenderable report is logged, not queued"
        );
        assert!(
            report_payload(&state, &orphan).await.is_err(),
            "and the payload read is what fails"
        );
    }

    #[tokio::test]
    async fn an_unreadable_database_queues_nothing_and_never_panics() {
        let state = dead_state();
        let row = ReportRow {
            id: fresh_id(),
            reporter_id: fresh_id(),
            target_id: fresh_id(),
            comment: String::new(),
            category: Category::Other,
            forwarded: false,
            state: report::ReportState::Open,
            assigned_to: None,
            action_taken_at: None,
            action_taken_by: None,
            rule_ids: vec![],
            ap_id: None,
            inserted_at: Utc::now(),
            updated_at: Utc::now(),
        };
        report_created(&state, &row).await;
        emit(&state, EVENT_ACCOUNT_CREATED, json!({})).await;
        assert!(
            report_payload(&state, &row).await.is_err(),
            "the report read fails against a pool that cannot connect"
        );
    }

    #[tokio::test]
    async fn the_account_hook_announces_the_admin_account_render() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let account = local(&pool, "whnewbie").await;
        let _hook = hook(&pool, "https://example.org/new", &[EVENT_ACCOUNT_CREATED]).await;

        account_created(&state, &account).await;

        let queued = deliveries(&pool).await;
        assert_eq!(queued.len(), 1, "the subscriber got the event");
        let job = queued.first().expect("one job");
        assert_eq!(job.args["event"], EVENT_ACCOUNT_CREATED);
        let data = &job.args["data"];
        assert_eq!(data["account"]["id"], account.id.to_string(), "the id");
        assert_eq!(data["account"]["nickname"], "whnewbie", "the nickname");
        assert_eq!(
            data["account"]["email"],
            json!("whnewbie@example.org"),
            "the admin render carries the address registration stored"
        );
        assert_eq!(
            data["account"]["roles"]["admin"], false,
            "a newbie is no admin"
        );
    }

    #[tokio::test]
    async fn an_unknown_event_queues_nothing_and_the_account_event_does() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let account = local(&pool, "quiet").await;
        let _hook = hook(&pool, "https://example.org/quiet", &[EVENT_ACCOUNT_CREATED]).await;
        // An event no webhook subscribes to (and none can name: it is off
        // `KNOWN_EVENTS`) fans out to nothing at all.
        emit(&state, "report.deleted", json!({"nothing": true})).await;

        account_created(&state, &account).await;
        assert_eq!(
            deliveries(&pool).await.len(),
            1,
            "the unknown event queued nothing, the account event queued one job"
        );
    }
}
