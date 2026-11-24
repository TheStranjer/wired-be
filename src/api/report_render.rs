//! The batched reads the two admin report renderers share.
//!
//! [`crate::api::admin::reports`] (Pleroma's `/api/pleroma/admin/reports`)
//! and [`crate::api::mastodon_admin_reports`] (Mastodon's
//! `/api/v1/admin/reports`) both name accounts and cite statuses per report,
//! and both do it in a fixed number of reads however long the page is.

use std::{collections::HashMap, hash::BuildHasher};

use uuid::Uuid;

use crate::{
    entities::status::Status,
    error::{AppError, AppResult},
    models::{
        report::{self, Report as ReportRow},
        status,
        user::{self, User},
    },
    services::render_status::{ViewerMode, render_statuses},
    state::AppState,
    util::collections::group_pairs,
};

/// Every account the reports name — reporter, target, assignee and acting
/// moderator, deduplicated — keyed by id in one read.
///
/// # Errors
///
/// [`AppError::Database`] from the read.
pub async fn report_accounts(
    state: &AppState,
    rows: &[ReportRow],
) -> AppResult<HashMap<Uuid, User>> {
    let mut ids: Vec<Uuid> = Vec::with_capacity(rows.len() * 4);
    for row in rows {
        ids.push(row.reporter_id);
        ids.push(row.target_id);
        ids.extend(row.assigned_to);
        ids.extend(row.action_taken_by);
    }
    let users = user::get_many(&state.db, &ids).await?;
    Ok(users.into_iter().map(|user| (user.id, user)).collect())
}

/// The account an entity slot names; its absence is the impossible state
/// the reports' foreign keys rule out, not a client failure.
///
/// # Errors
///
/// [`AppError::Internal`] as described.
pub fn report_account<S: BuildHasher>(
    users: &HashMap<Uuid, User, S>,
    id: Uuid,
) -> AppResult<&User> {
    users.get(&id).ok_or_else(|| {
        AppError::Internal(format!(
            "a report names the account {id}, which no users row holds"
        ))
    })
}

/// The rendered `statuses` arrays, keyed per report.
///
/// The citations come in one batched read, the rows behind them in one
/// [`status::get_many`], and each report's own list through
/// [`render_statuses`] with `viewer` as the viewing account and no
/// visibility filter on the rows. A report that cites nothing has no entry.
///
/// # Errors
///
/// [`AppError::Database`] from any read.
pub async fn cited_statuses(
    state: &AppState,
    viewer: &User,
    rows: &[ReportRow],
) -> AppResult<HashMap<Uuid, Vec<Status>>> {
    let report_ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let cited = report::statuses_for_reports(&state.db, &report_ids).await?;
    let status_ids: Vec<Uuid> = cited.iter().map(|&(_, status_id)| status_id).collect();
    let ids_per_report = group_pairs(cited);
    let status_rows = status::get_many(&state.db, &status_ids).await?;
    let by_id: HashMap<Uuid, &status::Status> =
        status_rows.iter().map(|row| (row.id, row)).collect();
    let mut rendered: HashMap<Uuid, Vec<Status>> = HashMap::new();
    for (report_id, ids) in ids_per_report {
        let owned: Vec<status::Status> = ids
            .iter()
            .filter_map(|id| by_id.get(id).copied())
            .cloned()
            .collect();
        let statuses = render_statuses(state, ViewerMode::user(viewer), &owned).await?;
        let _kept: Option<Vec<Status>> = rendered.insert(report_id, statuses);
    }
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use crate::{
        config::{Config, TEST_BASE_URL},
        models::{
            report::{Category, NewReport, ReportState, assign, create, set_state},
            status::{NewStatus, insert as insert_status},
            user::{NewUser, insert as insert_user, local_ap_id},
        },
    };

    use super::*;

    /// A local user named `nickname`.
    async fn local_user(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// A public post by `author`.
    async fn post(pool: &PgPool, author: &User, text: &str) -> status::Status {
        insert_status(pool, TEST_BASE_URL, &NewStatus::local(author, text))
            .await
            .expect("the status fixture inserts")
    }

    /// `reporter`'s report about `target` citing `status_ids`.
    async fn file(
        pool: &PgPool,
        reporter: &User,
        target: &User,
        status_ids: Vec<Uuid>,
    ) -> ReportRow {
        create(
            pool,
            TEST_BASE_URL,
            reporter,
            target,
            &NewReport {
                comment: String::new(),
                category: Category::Other,
                forwarded: false,
                rule_ids: Vec::new(),
                status_ids,
            },
        )
        .await
        .expect("the fixture report files")
    }

    /// The ids of the rendered entities, in order.
    fn ids_of(statuses: &[Status]) -> Vec<String> {
        statuses.iter().map(|entity| entity.id.clone()).collect()
    }

    /// Two reports citing overlapping statuses each get their own list,
    /// oldest status first, and a report citing nothing has no entry.
    #[tokio::test]
    async fn each_report_gets_its_own_ordered_citations() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = AppState::for_tests(db.pool().clone(), Config::for_tests());
        let alice = local_user(&state.db, "alice").await;
        let bob = local_user(&state.db, "bob").await;
        let carol = local_user(&state.db, "carol").await;
        let first = post(&state.db, &bob, "first").await;
        let second = post(&state.db, &bob, "second").await;
        let third = post(&state.db, &bob, "third").await;

        let wide = file(&state.db, &alice, &bob, vec![third.id, first.id, second.id]).await;
        let narrow = file(&state.db, &carol, &bob, vec![second.id, third.id]).await;
        let silent = file(&state.db, &carol, &alice, Vec::new()).await;

        let rows = vec![wide.clone(), narrow.clone(), silent.clone()];
        let cited = cited_statuses(&state, &alice, &rows)
            .await
            .expect("the citations render");

        assert_eq!(
            ids_of(&cited[&wide.id]),
            vec![
                first.id.to_string(),
                second.id.to_string(),
                third.id.to_string()
            ],
            "the wide report's three, oldest first"
        );
        assert_eq!(
            ids_of(&cited[&narrow.id]),
            vec![second.id.to_string(), third.id.to_string()],
            "the narrow report's two, though both are shared with the wide one"
        );
        assert!(
            cited.get(&silent.id).is_none_or(Vec::is_empty),
            "a report citing nothing has no statuses"
        );
        assert_eq!(cited.len(), 2, "and only the citing reports have entries");
    }

    /// No rows read nothing and map to nothing, for both batches.
    #[tokio::test]
    async fn no_reports_map_to_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = AppState::for_tests(db.pool().clone(), Config::for_tests());
        let alice = local_user(&state.db, "alice").await;

        let cited = cited_statuses(&state, &alice, &[])
            .await
            .expect("the empty page renders");
        assert!(cited.is_empty(), "no reports, no citations");
        let users = report_accounts(&state, &[])
            .await
            .expect("the empty page reads");
        assert!(users.is_empty(), "and no accounts");
    }

    /// The accounts map holds the reporter, the target, the assignee and
    /// the acting moderator, and [`report_account`] answers each by id and
    /// refuses an id the map lacks.
    #[tokio::test]
    async fn the_accounts_map_holds_all_four_roles() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = AppState::for_tests(db.pool().clone(), Config::for_tests());
        let reporter = local_user(&state.db, "reporter").await;
        let target = local_user(&state.db, "target").await;
        let assignee = local_user(&state.db, "assignee").await;
        let actor = local_user(&state.db, "actor").await;
        let stranger = local_user(&state.db, "stranger").await;

        let filed = file(&state.db, &reporter, &target, Vec::new()).await;
        let _assigned = assign(&state.db, filed.id, Some(assignee.id))
            .await
            .expect("the assignment writes")
            .expect("the report exists");
        let _resolved = set_state(&state.db, filed.id, ReportState::Resolved, Some(actor.id))
            .await
            .expect("the resolution writes")
            .expect("the report exists");
        let row = report::get(&state.db, filed.id)
            .await
            .expect("the row reads")
            .expect("the report exists");

        let users = report_accounts(&state, std::slice::from_ref(&row))
            .await
            .expect("the accounts read");

        assert_eq!(users.len(), 4, "four roles, four accounts");
        for (role, account) in [
            ("reporter", &reporter),
            ("target", &target),
            ("assignee", &assignee),
            ("actor", &actor),
        ] {
            assert_eq!(
                report_account(&users, account.id)
                    .unwrap_or_else(|err| panic!("the {role} is named: {err}"))
                    .id,
                account.id,
                "the {role} comes back under its own id"
            );
        }
        let err = report_account(&users, stranger.id).expect_err("an unnamed account is absent");
        assert!(
            matches!(err, AppError::Internal(_)),
            "an id the map lacks is the internal error: {err}"
        );
    }
}
