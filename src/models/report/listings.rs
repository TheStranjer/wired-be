//! Walking the queue: [`list`] is the moderation listing's unfiltered
//! keyset page, [`admin_list`] is Pleroma's admin queue with its state, rule
//! and assignee filters over an offset window, and [`statuses_for_reports`]
//! is the batched citation read that renders a whole page's context in one
//! query.

use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::AppResult,
    pagination::{PageParams, keyset_page},
};

use super::{Report, ReportState};

/// [`list`]: every report, newest first. The statement keeps its `WHERE`
/// clause even unfiltered because [`PageParams::apply_keyset`] appends its
/// cursors to it. The admin listing's filters — state, rule, assignee — are
/// [`admin_list`]'s own statement, which pages by offset window.
const LIST_SQL: &str = "SELECT * FROM reports WHERE true";

/// The start of [`admin_list`]'s page statement; [`push_admin_filters`]
/// appends the `WHERE` clause, then the newest-first order and the
/// `LIMIT`/`OFFSET` window the admin report listing pages by (the same
/// offset walk the moderation log takes, keyed on the report's own id).
const ADMIN_SELECT_SQL: &str = "SELECT * FROM reports";

/// The start of the total statement behind [`admin_list`]: the same
/// filters, counted without the window.
const ADMIN_COUNT_SQL: &str = "SELECT count(*) FROM reports";

/// [`statuses_for_reports`]: the batched twin of [`statuses_for`](super::statuses_for) — the
/// same live-status join and the same oldest-first order, for a whole
/// page of reports in one read, keyed `(report_id, status_id)`.
const STATUSES_FOR_MANY_SQL: &str = "SELECT rs.report_id, rs.status_id
     FROM report_statuses rs
     JOIN statuses s ON s.id = rs.status_id AND s.deleted_at IS NULL
     WHERE rs.report_id = ANY($1)
     ORDER BY rs.report_id, rs.status_id";

/// The filters and window of one [`admin_list`] read, as
/// `GET /api/v1/pleroma/admin/reports` parses them (Pleroma's
/// `ReportController.index` params through `Utils.get_reports`).
#[derive(Debug)]
pub struct AdminListParams {
    /// Only reports in this state; `None` is every state.
    pub state: Option<ReportState>,
    /// Only reports citing this instance rule (Pleroma's `rule_id` filter
    /// over the Flag's `context.rules`, here the row's own
    /// [`rule_ids`](Report::rule_ids) array).
    pub rule_id: Option<Uuid>,
    /// Only reports assigned to this account (Pleroma's
    /// `restrict_assigned_account`, which matches the id stored on the
    /// Flag; here the [`assigned_to`](Report::assigned_to) column).
    pub assigned_account: Option<Uuid>,
    /// One-based page number; the caller defaults and validates it (1).
    pub page: i64,
    /// Rows per page; the caller defaults and caps it.
    pub page_size: i64,
}

/// The page [`admin_list`] answers: one window of reports, newest filing
/// first, and the total number of rows the same filters select.
#[derive(Debug)]
pub struct AdminListPage {
    /// The reports of this page.
    pub reports: Vec<Report>,
    /// How many reports the filters match, pages included.
    pub total: i64,
}

/// Every report, newest first and `page`'s `limit` at a time.
///
/// The moderation queue's unfiltered walk, keyed on the report row's own id
/// (decision 4). The admin listing's filters — by [`ReportState`], by
/// [`assigned_to`](Report::assigned_to), by cited rule — live in
/// [`admin_list`], which is offset-paged the way the admin API pages.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list(pool: &PgPool, page: PageParams) -> AppResult<Vec<Report>> {
    keyset_page(pool, QueryBuilder::<Postgres>::new(LIST_SQL), "id", &page).await
}

/// Every report matching `params`' filters, newest filing first, one
/// `page`/`page_size` window of them beside the total the filters select.
///
/// The admin moderation queue's walk (Pleroma's `Utils.get_reports`, whose
/// state / rule / assigned-account filters and offset window these are):
/// the ordering and the keyset are the ones [`list`] already states, with
/// the offset paging the admin listings use on top.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn admin_list(pool: &PgPool, params: &AdminListParams) -> AppResult<AdminListPage> {
    let total = count_admin_matching(pool, params).await?;
    let offset = (params.page - 1).saturating_mul(params.page_size);
    let mut builder = QueryBuilder::<Postgres>::new(ADMIN_SELECT_SQL);
    push_admin_filters(&mut builder, params);
    let _window: &mut _ = builder
        .push(" ORDER BY id DESC LIMIT ")
        .push_bind(params.page_size)
        .push(" OFFSET ")
        .push_bind(offset);
    let reports = builder.build_query_as::<Report>().fetch_all(pool).await?;
    Ok(AdminListPage { reports, total })
}

/// The number of rows `params`' filters select, whatever page they land on.
async fn count_admin_matching(pool: &PgPool, params: &AdminListParams) -> AppResult<i64> {
    let mut builder = QueryBuilder::<Postgres>::new(ADMIN_COUNT_SQL);
    push_admin_filters(&mut builder, params);
    let total = builder.build_query_scalar::<i64>().fetch_one(pool).await?;
    Ok(total)
}

/// Appends the shared `WHERE` clause of [`admin_list`] and
/// [`count_admin_matching`]: the state on its column, the rule as a `@>`
/// containment on the [`rule_ids`](Report::rule_ids) array (Pleroma's
/// `selected_rule_ids && [rule_id]` intersection), and the assignee on
/// [`assigned_to`](Report::assigned_to). The always-true predicate leads so
/// every filter is one more `AND`.
fn push_admin_filters(builder: &mut QueryBuilder<Postgres>, params: &AdminListParams) {
    let _where: &mut _ = builder.push(" WHERE true");
    if let Some(state) = params.state {
        let _state: &mut _ = builder.push(" AND state = ").push_bind(state);
    }
    if let Some(rule_id) = params.rule_id {
        let _rule: &mut _ = builder.push(" AND rule_ids @> ").push_bind(vec![rule_id]);
    }
    if let Some(assigned) = params.assigned_account {
        let _assignee: &mut _ = builder.push(" AND assigned_to = ").push_bind(assigned);
    }
}

/// The ids of the live statuses the reports in `report_ids` cite, as
/// `(report_id, status_id)` pairs ordered by report and then by status.
///
/// [`statuses_for`](super::statuses_for) for a whole page in one read, with the same
/// live-status join and the same oldest-status-first order.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn statuses_for_reports(
    pool: &PgPool,
    report_ids: &[Uuid],
) -> AppResult<Vec<(Uuid, Uuid)>> {
    if report_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as::<_, (Uuid, Uuid)>(STATUSES_FOR_MANY_SQL)
        .bind(report_ids)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use crate::{
        config::TEST_BASE_URL,
        models::{
            instance_rule,
            report::{NewReport, ReportState, assign, create, set_state, test_support::*},
        },
    };

    use super::*;

    /// The moderation queue walks newest filing first and by report id, the
    /// cursors [`PageParams`] speaks.
    #[tokio::test]
    async fn a_page_of_reports_newest_first_walks_by_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        for nickname in ["bob", "carol", "dave"] {
            let target = local_user(&pool, nickname).await;
            let _filing = seed(&pool, &alice, &target).await;
        }
        let stored = stored_rows(&pool).await;
        assert_eq!(stored.len(), 3, "three fixture reports");

        let first = list(&pool, page("limit=2")).await.unwrap();
        assert_eq!(
            ids_of(&first),
            vec![stored[0].id, stored[1].id],
            "the two newest filings"
        );

        let older = list(&pool, page(&format!("max_id={}", stored[1].id)))
            .await
            .unwrap();
        assert_eq!(
            ids_of(&older),
            vec![stored[2].id],
            "max_id continues where the page ended"
        );

        let newer = list(&pool, page(&format!("min_id={}", stored[2].id)))
            .await
            .unwrap();
        assert_eq!(
            ids_of(&newer),
            vec![stored[0].id, stored[1].id],
            "a min_id page is answered newest-first too"
        );

        let tail = list(&pool, page(&format!("max_id={}", stored[2].id)))
            .await
            .unwrap();
        assert!(tail.is_empty(), "walking off the end is no error");
    }

    /// The listing params with no filter: [`admin_list`]'s plain walk of
    /// the whole queue.
    fn listing() -> AdminListParams {
        AdminListParams {
            state: None,
            rule_id: None,
            assigned_account: None,
            page: 1,
            page_size: 20,
        }
    }

    /// The report ids [`admin_list`] answers for `params`, in order.
    async fn listed_ids(pool: &PgPool, params: &AdminListParams) -> Vec<Uuid> {
        let page = admin_list(pool, params).await.expect("the listing runs");
        ids_of(&page.reports)
    }

    /// [`admin_list`] walks newest filing first and windows by `page` /
    /// `page_size` beside the unwindowed total; an off-the-end page is
    /// empty without shrinking the count.
    #[tokio::test]
    async fn the_admin_listing_windows_the_queue() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        for nickname in ["carol", "dave", "erin"] {
            let target = local_user(&pool, nickname).await;
            let _filing = seed(&pool, &alice, &target).await;
        }
        let _last = seed(&pool, &bob, &alice).await;
        let stored: Vec<Uuid> =
            sqlx::query_as::<_, (Uuid,)>("SELECT id FROM reports ORDER BY id DESC")
                .fetch_all(&pool)
                .await
                .expect("the stored ids read")
                .into_iter()
                .map(|(id,)| id)
                .collect();
        let all = listing();

        let page = admin_list(&pool, &all).await.unwrap();
        assert_eq!(page.total, 4, "the whole table");
        assert_eq!(ids_of(&page.reports), stored, "newest filing first");

        let window = AdminListParams {
            page_size: 2,
            ..all
        };
        assert_eq!(
            listed_ids(&pool, &window).await,
            stored[..2],
            "the first two"
        );

        let second = AdminListParams {
            page: 2,
            page_size: 2,
            ..all
        };
        assert_eq!(
            listed_ids(&pool, &second).await,
            stored[2..],
            "the next page walks the window"
        );

        let past = AdminListParams {
            page: 3,
            page_size: 2,
            ..all
        };
        assert!(listed_ids(&pool, &past).await.is_empty(), "off the end");
        let counted = admin_list(&pool, &past).await.unwrap();
        assert_eq!(counted.total, 4, "still counting all");
    }

    /// [`admin_list`]'s three filters each keep exactly their own
    /// reports: the state on the `state` column, the cited rule on the
    /// `rule_ids` containment, the assignee on the `assigned_to` column.
    #[tokio::test]
    async fn the_admin_listing_filters_by_state_rule_and_assignee() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let moddie = local_user(&pool, "moddie").await;
        let rule = instance_rule::create(&pool, "No harassment", "", 1)
            .await
            .expect("the rule fixture inserts");
        let first = seed(&pool, &alice, &bob).await;
        let citing = create(
            &pool,
            TEST_BASE_URL,
            &alice,
            &bob,
            &NewReport {
                rule_ids: vec![rule.id],
                ..bare()
            },
        )
        .await
        .expect("the citing report files");
        let closed = seed(&pool, &alice, &bob).await;
        let _moved = set_state(&pool, closed.id, ReportState::Closed, None)
            .await
            .unwrap()
            .expect("the report exists");
        let _assigned = assign(&pool, first.id, Some(moddie.id))
            .await
            .unwrap()
            .expect("the report exists");
        let all = listing();

        let by_state = AdminListParams {
            state: Some(ReportState::Closed),
            ..all
        };
        assert_eq!(
            listed_ids(&pool, &by_state).await,
            vec![closed.id],
            "only the closed filing"
        );
        let by_rule = AdminListParams {
            rule_id: Some(rule.id),
            ..all
        };
        assert_eq!(
            listed_ids(&pool, &by_rule).await,
            vec![citing.id],
            "only the report citing the rule"
        );
        let by_assignee = AdminListParams {
            assigned_account: Some(moddie.id),
            ..all
        };
        assert_eq!(
            listed_ids(&pool, &by_assignee).await,
            vec![first.id],
            "only the assigned report"
        );
    }

    /// The batched citation read answers the same live-only, oldest-first
    /// context as [`statuses_for`], keyed per report, and an empty id list
    /// reads nothing.
    #[tokio::test]
    async fn the_batched_citation_read_groups_the_live_statuses() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        let kept = post(&pool, &bob, "kept").await;
        let doomed = post(&pool, &bob, "doomed").await;
        let citing = create(
            &pool,
            TEST_BASE_URL,
            &alice,
            &bob,
            &NewReport {
                status_ids: vec![doomed.id, kept.id],
                ..bare()
            },
        )
        .await
        .expect("the fixture report files");
        let plain = seed(&pool, &carol, &alice).await;

        let pairs = statuses_for_reports(&pool, &[citing.id, plain.id])
            .await
            .unwrap();
        assert_eq!(
            pairs,
            vec![(citing.id, kept.id), (citing.id, doomed.id)],
            "the citing report's pair, oldest status first"
        );

        let _gone = sqlx::query("UPDATE statuses SET deleted_at = now() WHERE id = $1")
            .bind(doomed.id)
            .execute(&pool)
            .await
            .expect("the fixture deletes the post");
        let after = statuses_for_reports(&pool, &[citing.id, plain.id])
            .await
            .unwrap();
        assert_eq!(
            after,
            vec![(citing.id, kept.id)],
            "the deleted post leaves the batched answer too"
        );
        assert!(
            statuses_for_reports(&pool, &[]).await.unwrap().is_empty(),
            "no ids read nothing"
        );
    }
}
