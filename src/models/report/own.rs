//! The reporter's own view of the ledger: [`list_for_reporter`] pages the
//! reports one account filed, newest first, beside their total.

use sqlx::PgPool;
use uuid::Uuid;

use super::Report;
use crate::error::AppResult;

/// The page size [`list_for_reporter`]'s callers use when a request names
/// none.
pub const DEFAULT_OWN_PAGE_SIZE: i64 = 20;
/// The largest page [`list_for_reporter`] serves; a bigger request is
/// clamped to it.
pub const MAX_OWN_PAGE_SIZE: i64 = 200;

/// [`list_for_reporter`]: one window of the reporter's reports, newest
/// filing first (the `UUIDv7` primary key orders by creation).
const OWN_PAGE_SQL: &str =
    "SELECT * FROM reports WHERE reporter_id = $1 ORDER BY id DESC LIMIT $2 OFFSET $3";

/// [`list_for_reporter`]: how many reports the reporter filed in all.
const OWN_COUNT_SQL: &str = "SELECT count(*) FROM reports WHERE reporter_id = $1";

/// The page [`list_for_reporter`] answers.
#[derive(Debug)]
pub struct OwnReportPage {
    /// The reports of this page, newest filing first.
    pub reports: Vec<Report>,
    /// How many reports the reporter filed, pages included.
    pub total: i64,
}

/// The reports `reporter_id` filed, newest first: the `size`-row window at
/// one-based `page`, beside the total.
///
/// `page` below 1 reads as 1 and `size` is clamped to `1..=`
/// [`MAX_OWN_PAGE_SIZE`], so no caller can ask for an empty or unbounded
/// window. This is Pleroma's `PleromaAPI.ReportController.index`, which
/// forces `actor_id` to the caller before it lists.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list_for_reporter(
    pool: &PgPool,
    reporter_id: Uuid,
    page: i64,
    size: i64,
) -> AppResult<OwnReportPage> {
    let size = size.clamp(1, MAX_OWN_PAGE_SIZE);
    let offset = (page.max(1) - 1).saturating_mul(size);
    let total = sqlx::query_scalar::<_, i64>(OWN_COUNT_SQL)
        .bind(reporter_id)
        .fetch_one(pool)
        .await?;
    let reports = sqlx::query_as::<_, Report>(OWN_PAGE_SQL)
        .bind(reporter_id)
        .bind(size)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    Ok(OwnReportPage { reports, total })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;

    /// Only the reporter's own rows are listed, newest first, and the total
    /// counts just those.
    #[tokio::test]
    async fn only_the_reporters_own_reports_are_listed_newest_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        let _first = seed(&pool, &alice, &carol).await;
        let _other = seed(&pool, &bob, &carol).await;
        let _second = seed(&pool, &alice, &bob).await;
        let expected: Vec<Uuid> = stored_rows(&pool)
            .await
            .into_iter()
            .filter(|row| row.reporter_id == alice.id)
            .map(|row| row.id)
            .collect();

        let page = list_for_reporter(&pool, alice.id, 1, DEFAULT_OWN_PAGE_SIZE)
            .await
            .expect("the listing reads");

        assert_eq!(page.total, 2, "alice filed two");
        assert_eq!(ids_of(&page.reports), expected, "hers, newest first");
        let nobody = list_for_reporter(&pool, carol.id, 1, DEFAULT_OWN_PAGE_SIZE)
            .await
            .expect("the listing reads");
        assert_eq!(nobody.total, 0, "carol filed none");
        assert!(nobody.reports.is_empty(), "and lists none");
    }

    /// The window pages by one-based page number, and a page or size out of
    /// range is clamped rather than refused.
    #[tokio::test]
    async fn the_window_pages_and_clamps_its_inputs() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        for _ in 0..3 {
            let _filing = seed(&pool, &alice, &bob).await;
        }
        let all = ids_of(
            &list_for_reporter(&pool, alice.id, 1, MAX_OWN_PAGE_SIZE)
                .await
                .expect("the full page reads")
                .reports,
        );
        assert_eq!(all.len(), 3, "three filings");

        let first = list_for_reporter(&pool, alice.id, 1, 2).await.unwrap();
        assert_eq!(ids_of(&first.reports), all[..2], "page one holds two");
        assert_eq!(first.total, 3, "the total ignores the window");
        let second = list_for_reporter(&pool, alice.id, 2, 2).await.unwrap();
        assert_eq!(ids_of(&second.reports), all[2..], "page two holds the rest");
        let past = list_for_reporter(&pool, alice.id, 3, 2).await.unwrap();
        assert!(past.reports.is_empty(), "a page past the end is empty");

        let zero = list_for_reporter(&pool, alice.id, 0, 2).await.unwrap();
        assert_eq!(ids_of(&zero.reports), all[..2], "page 0 reads as page 1");
        let empty_window = list_for_reporter(&pool, alice.id, 1, 0).await.unwrap();
        assert_eq!(empty_window.reports.len(), 1, "a size of 0 reads as 1");
        let huge = list_for_reporter(&pool, alice.id, 1, i64::MAX)
            .await
            .unwrap();
        assert_eq!(huge.reports.len(), 3, "a huge size is clamped, not refused");
    }
}
