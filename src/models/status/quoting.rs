//! The quoting page read: the live rows that quote one original, newest id
//! first ([`quoting_page`]) — the `quotes` counterpart of
//! [`super::reblogger_page`], keyed the same way on the row that quotes.

use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{error::AppResult, pagination::PageParams};

use super::Status;

/// [`quoting_page`]: the live rows quoting one original, newest id first.
/// As in
/// [`REBLOGGERS_PAGE_SQL`](super::reblogs::REBLOGGERS_PAGE_SQL), the text stops
/// where its one placeholder
/// belongs, because that is where [`quoting_page`] pushes its bind; the
/// live-row filter and the keyset then run on the quoter row id `s.id`.
const QUOTING_PAGE_SQL: &str = "SELECT s.* FROM statuses s
      WHERE s.quote_of_id = ";

/// The statuses quoting `quoted_id`, newest quote first and `page`'s
/// `limit` at a time, as full [`Status`] rows.
///
/// This is the raw read behind a `GET /api/v1/statuses/{id}/quotes` style
/// listing (Pleroma walks the quoted object's quoting activities in
/// `lib/pleroma/web/common_api.ex`): the page keys on the *quoter row* id,
/// so its `max_id`/`min_id`/`since_id` cursors and `Link` header carry
/// quoter row ids (decision 4: `UUIDv7` text order is creation order). A
/// tombstoned quote is off the page — a deleted quoter quotes nothing.
/// Which of the rows a given viewer may see is the caller's
/// [`crate::models::status_visibility`] check, not this query's, per the
/// `render_statuses` contract: this answers live rows, capped at the page
/// limit, unfiltered.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn quoting_page(
    pool: &PgPool,
    quoted_id: Uuid,
    page: PageParams,
) -> AppResult<Vec<Status>> {
    let mut builder = QueryBuilder::<Postgres>::new(QUOTING_PAGE_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _bound: &mut _ = builder.push_bind(quoted_id);
    let _live: &mut _ = builder.push(" AND s.deleted_at IS NULL");
    page.apply_keyset(&mut builder, "s.id");
    let statuses = builder.build_query_as::<Status>().fetch_all(pool).await?;
    Ok(page.finish(statuses))
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::models::status::{soft_delete, test_support::*};
    use crate::util::id::new_id;

    /// The live quoting rows of `quoted`, newest id first, straight from the
    /// database: two `UUIDv7`s minted in one millisecond sort by their
    /// random bits, not by the moment they were asked for, so the quoting
    /// page tests take their expected order from the store, not from the
    /// order the fixtures ran in.
    async fn live_quoter_ids(pool: &PgPool, quoted: Uuid) -> Vec<Uuid> {
        sqlx::query_as::<_, (Uuid,)>(
            "SELECT id FROM statuses
              WHERE quote_of_id = $1 AND deleted_at IS NULL
           ORDER BY id DESC",
        )
        .bind(quoted)
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|(id,)| id)
        .collect()
    }

    /// An original quoted three times by fresh accounts, with the database
    /// order of those live quoting rows newest-first.
    async fn quoted_three_times(pool: &PgPool) -> (Status, Vec<Uuid>) {
        let alice = user(pool, "alice").await;
        let original = post(pool, &alice, "quote me").await;
        for n in 0..3 {
            let author = user(pool, &format!("quoter{n}")).await;
            let _quote = quote(pool, &author, original.id, &format!("quote {n}")).await;
        }
        let all = live_quoter_ids(pool, original.id).await;
        assert_eq!(all.len(), 3, "three live quotes of the one original");
        (original, all)
    }

    /// [`quoting_page`] answers full live [`Status`] rows keyed on the
    /// quoter row id, newest first: the limit cuts the page and `max_id`
    /// walks it, exactly as the reblogger page's do — and another status's
    /// quote is on no page.
    #[tokio::test]
    async fn the_quoting_page_walks_the_quoter_row_ids() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (original, all) = quoted_three_times(&pool).await;
        let carol = user(&pool, "carol").await;
        let other = post(&pool, &carol, "quoted once").await;
        let lonely = user(&pool, "lonely").await;
        let _elsewhere = quote(&pool, &lonely, other.id, "somewhere else").await;

        assert_eq!(
            ids(&quoting_page(&pool, original.id, page("")).await.unwrap()),
            all,
            "every live quoter, newest first, as full rows"
        );
        assert_eq!(
            ids(&quoting_page(&pool, original.id, page("limit=2"))
                .await
                .unwrap()),
            vec![all[0], all[1]],
            "the limit cuts the page"
        );
        assert_eq!(
            ids(
                &quoting_page(&pool, original.id, page(&format!("max_id={}", all[1])))
                    .await
                    .unwrap()
            ),
            vec![all[2]],
            "max_id continues where the page ended, and `other`'s quote is on no page"
        );
        assert!(
            quoting_page(&pool, original.id, page(&format!("max_id={}", all[2])))
                .await
                .unwrap()
                .is_empty(),
            "past the oldest quote the page is empty"
        );
    }

    /// The same page read the other way and after a loss: `min_id` answers
    /// the newer page flipped newest-first, `since_id` polls descending
    /// like a first page, a tombstoned quoter drops off the page, and a
    /// status nobody quotes (or that is not stored at all) is the empty
    /// page.
    #[tokio::test]
    async fn the_quoting_page_flips_newer_pages_and_drops_tombstones() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (original, all) = quoted_three_times(&pool).await;
        let carol = user(&pool, "carol").await;
        let silent = post(&pool, &carol, "nobody quotes this").await;

        assert_eq!(
            ids(
                &quoting_page(&pool, original.id, page(&format!("min_id={}", all[2])))
                    .await
                    .unwrap()
            ),
            vec![all[0], all[1]],
            "min_id answers the newer page, newest-first after the flip"
        );
        assert_eq!(
            ids(
                &quoting_page(&pool, original.id, page(&format!("since_id={}", all[1])))
                    .await
                    .unwrap()
            ),
            vec![all[0]],
            "since_id polls the newer rows, descending like a first page"
        );

        assert!(
            soft_delete(&pool, all[1]).await.unwrap().is_some(),
            "the middle quoter deletes their quote"
        );
        assert_eq!(
            ids(&quoting_page(&pool, original.id, page("")).await.unwrap()),
            vec![all[0], all[2]],
            "a tombstoned quoter leaves the page"
        );
        assert!(
            quoting_page(&pool, silent.id, page(""))
                .await
                .unwrap()
                .is_empty(),
            "a status nobody quotes is the empty page"
        );
        assert!(
            quoting_page(&pool, new_id(), page(""))
                .await
                .unwrap()
                .is_empty(),
            "an unknown id has no quoters either"
        );
    }
}
