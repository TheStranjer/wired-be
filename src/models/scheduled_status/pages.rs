//! One account's schedules as a keyset page: [`page_for_user`] walks the
//! account's rows newest-id-first, the shape Pleroma's scheduled-activity
//! listing answers with. The ordering, the cursors and the limit all live
//! on `id`; the read that genuinely needs the due-time order is
//! [`fetch_due`](super::fetch_due).

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    error::AppResult,
    pagination::{PageParams, keyset_page_by},
};

use super::ScheduledStatus;

/// [`page_for_user`]: the start of one account's schedule listing — the
/// owner bound first, then [`PageParams`]' keyset conditions, ordering and
/// `LIMIT` on `id` appended after it. The ordering is the id column's, not
/// the fire moment's; see [`page_for_user`] for why.
///
/// [`PageParams`]: crate::pagination::PageParams
const PAGE_SQL: &str = "SELECT * FROM scheduled_statuses WHERE user_id = ";

/// One account's schedules as a [`PageParams`] keyset page, newest-created
/// first.
///
/// This is Pleroma's `MastodonAPI.get_scheduled_activities`:
/// `ScheduledActivity.for_user_query` through `Pagination.fetch_paginated`,
/// which carries no ordering of its own, so Pleroma's default `id desc` is
/// what the listing answers. The keyset, the cursors and the `Link` header all
/// live on `id`, and that is what fixes the order: a listing read in
/// [`scheduled_at`] order cannot be walked by id comparisons, so the time
/// order and the cursors cannot both hold. Pleroma settles it for the id
/// column, and since a `UUIDv7` is minted in creation order the page still
/// runs a burst of scheduling back through the order it was submitted in. For
/// the read that genuinely needs the due-time order there is [`fetch_due`].
///
/// `page.finish` is what turns the `min_id` page — read ascending off the
/// index — back into the newest-first order the response goes out in.
///
/// [`scheduled_at`]: ScheduledStatus::scheduled_at
/// [`fetch_due`]: super::fetch_due
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn page_for_user(
    pool: &PgPool,
    user_id: Uuid,
    page: PageParams,
) -> AppResult<Vec<ScheduledStatus>> {
    keyset_page_by(pool, PAGE_SQL, user_id, "id", &page).await
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::test_support::{at, schedule, user};
    use super::*;

    /// The page parameters of `query`, on the listing's cap.
    fn page(query: &str) -> PageParams {
        PageParams::from_query(Some(query), crate::pagination::MAX_LIMIT)
            .expect("the fixture query paginates")
    }

    /// The ids of `rows`, in the order they came back.
    fn ids(rows: &[ScheduledStatus]) -> Vec<Uuid> {
        rows.iter().map(|row| row.id).collect()
    }

    #[tokio::test]
    async fn page_for_user_walks_one_account_newest_id_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        // Created oldest-first with the due times the other way round: the
        // page is keyed on `id`, so the newest id leads no matter when the
        // row fires.
        let first = schedule(&pool, &alice, at(20, 0, 0), json!({ "status": "a" })).await;
        let second = schedule(&pool, &alice, at(19, 0, 0), json!({ "status": "b" })).await;
        let third = schedule(&pool, &alice, at(18, 0, 0), json!({ "status": "c" })).await;
        let theirs = schedule(&pool, &bob, at(17, 0, 0), json!({ "status": "d" })).await;

        let all = page_for_user(&pool, alice.id, page(""))
            .await
            .expect("the first page reads");
        assert_eq!(
            ids(&all),
            vec![third.id, second.id, first.id],
            "the account's own rows, newest id first, another account's left out"
        );

        let older = page_for_user(&pool, alice.id, page(&format!("max_id={}", second.id)))
            .await
            .expect("the cursor page reads");
        assert_eq!(
            ids(&older),
            vec![first.id],
            "max_id continues where the page ended"
        );

        let newer = page_for_user(&pool, alice.id, page(&format!("min_id={}", second.id)))
            .await
            .expect("the forward page reads");
        assert_eq!(
            ids(&newer),
            vec![third.id],
            "min_id answers the newer page, still newest first"
        );

        let unseen = page_for_user(&pool, alice.id, page(&format!("since_id={}", first.id)))
            .await
            .expect("the unseen page reads");
        assert_eq!(
            ids(&unseen),
            vec![third.id, second.id],
            "since_id reads everything made after its cursor"
        );

        let small = page_for_user(&pool, alice.id, page("limit=2"))
            .await
            .expect("the limited page reads");
        assert_eq!(
            ids(&small),
            vec![third.id, second.id],
            "and the limit cuts the newest page"
        );

        assert!(
            page_for_user(&pool, Uuid::now_v7(), page(""))
                .await
                .expect("a stranger's page reads")
                .is_empty(),
            "an account with no schedule lists nothing"
        );
        assert_eq!(
            ids(&page_for_user(&pool, bob.id, page(""))
                .await
                .expect("bob's page reads"),),
            vec![theirs.id],
            "the page is each account's own"
        );
    }
}
