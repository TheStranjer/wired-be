//! The `media_proxy_cache_urls` table: the ledger of remote URLs the
//! `/proxy` route has served (`docs/ARCHITECTURE.md` decision 1, WBE-208).
//!
//! Reference: Pleroma keeps this list in the
//! `:banned_urls_cache` Cachex table (`Pleroma.Web.MediaProxy`'s
//! `cache_table/0`, `lib/pleroma/web/media_proxy.ex`), streamed by
//! `MediaProxyCacheController.index` and cleared by its `delete`/`purge`
//! actions (`lib/pleroma/web/admin_api/controllers/media_proxy_cache_controller.ex`).
//! Here it is a real table — one row per served URL, keyed by the URL text
//! itself, so recording a re-served URL is an upsert (decision 2: plain
//! runtime-checked queries against it).
//!
//! [`record`] is what the media proxy route calls after it has served a
//! URL: the upsert, followed by the prune that keeps the table within
//! `MEDIA_PROXY_CACHE_LIMIT` rows by dropping the oldest `inserted_at`
//! first. [`list`] and [`count`] are the admin listing's window and total
//! (a case-insensitive substring filter on `query`, Pleroma's
//! `~r/#{query}/i` narrowed to what the ticket pins), and [`remove`] is
//! the delete/purge actions' write.

use sqlx::PgPool;

use crate::error::AppResult;

/// [`record`]'s upsert: the URL's row carries the moment it was last
/// served, so re-serving refreshes it and the prune never drops a live
/// entry ahead of a stale one.
const UPSERT_SQL: &str = "INSERT INTO media_proxy_cache_urls (url, inserted_at)
     VALUES ($1, now())
ON CONFLICT (url) DO UPDATE SET inserted_at = now()";

/// [`record`]'s prune: delete the oldest rows beyond `limit`, ordered by
/// `inserted_at` with `url` breaking exact ties so repeated runs are
/// deterministic. `GREATEST` floors the count at zero — with the table at
/// or under the limit the inner `LIMIT 0` deletes nothing.
const PRUNE_SQL: &str = "DELETE FROM media_proxy_cache_urls
      WHERE url IN (
        SELECT url FROM media_proxy_cache_urls
         ORDER BY inserted_at, url
            LIMIT GREATEST(
              (SELECT COUNT(*) FROM media_proxy_cache_urls) - $1::bigint,
              0
            )
      )";

/// [`list`]: one page of the filtered ledger in `url` order — the
/// listing's stable order, whatever the serving clock did. The filter is
/// the shared case-insensitive substring rule: absent (`NULL`) selects
/// everything, present is matched with `strpos` on folded text so `%` and
/// `_` are literal characters and not wildcards.
const LIST_SQL: &str = "SELECT url FROM media_proxy_cache_urls
      WHERE ($1::text IS NULL OR strpos(lower(url), lower($1)) > 0)
      ORDER BY url
      LIMIT $2 OFFSET $3";

/// [`count`]: how many rows the filter selects, pages included —
/// Pleroma's `count: length(entries)` over the whole filtered stream, with
/// the same `strpos` filter as [`LIST_SQL`].
const COUNT_SQL: &str = "SELECT COUNT(*)::bigint AS count
       FROM media_proxy_cache_urls
      WHERE ($1::text IS NULL OR strpos(lower(url), lower($1)) > 0)";

/// [`remove`]: the admin delete/purge write; every named URL goes, and
/// the affected count tells how many of them were actually stored.
const REMOVE_SQL: &str = "DELETE FROM media_proxy_cache_urls WHERE url = ANY($1)";

/// The row-count limit as the `bigint` the prune binds: a `usize` past
/// the column's range is a limit nothing can exceed, so `i64::MAX`.
fn limit_column(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

/// Records that `url` was just served: the upsert, then the prune down to
/// `limit` rows (the oldest `inserted_at` go first).
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn record(pool: &PgPool, url: &str, limit: usize) -> AppResult<()> {
    let _upserted = sqlx::query(UPSERT_SQL).bind(url).execute(pool).await?;
    let _pruned = sqlx::query(PRUNE_SQL)
        .bind(limit_column(limit))
        .execute(pool)
        .await?;
    Ok(())
}

/// One page of the ledger, `url`-ordered: at most `limit` rows starting at
/// `offset`, filtered by the optional case-insensitive substring `query`.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn list(
    pool: &PgPool,
    query: Option<&str>,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<String>> {
    let rows = sqlx::query_scalar::<_, String>(LIST_SQL)
        .bind(query)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// How many rows `query` selects, pages included.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn count(pool: &PgPool, query: Option<&str>) -> AppResult<i64> {
    let total = sqlx::query_scalar::<_, i64>(COUNT_SQL)
        .bind(query)
        .fetch_one(pool)
        .await?;
    Ok(total)
}

/// Removes every URL in `urls` from the ledger and reports how many rows
/// went; names that were never recorded affect nothing.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn remove(pool: &PgPool, urls: &[String]) -> AppResult<u64> {
    let outcome = sqlx::query(REMOVE_SQL).bind(urls).execute(pool).await?;
    Ok(outcome.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_db;

    /// A recorded ledger row named by its URL text.
    async fn seed(pool: &PgPool, url: &str) {
        record(pool, url, 100)
            .await
            .expect("recording a served url writes");
    }

    #[tokio::test]
    async fn record_upserts_one_row_per_url() {
        let db = test_db::test_db().await.expect("test database slot");
        let pool = db.pool();

        seed(pool, "https://remote.example/a.png").await;
        seed(pool, "https://remote.example/a.png").await;
        seed(pool, "https://remote.example/b.png").await;

        assert_eq!(
            count(pool, None).await.expect("the count reads"),
            2,
            "re-serving refreshes the row instead of stacking copies"
        );
    }

    #[tokio::test]
    async fn record_prunes_the_oldest_past_the_limit() {
        let db = test_db::test_db().await.expect("test database slot");
        let pool = db.pool();

        record(pool, "https://remote.example/a.png", 2)
            .await
            .expect("the first record");
        record(pool, "https://remote.example/b.png", 2)
            .await
            .expect("the second record");
        record(pool, "https://remote.example/c.png", 2)
            .await
            .expect("the third record prunes");

        assert_eq!(
            count(pool, None).await.expect("the count reads"),
            2,
            "the table stays at the limit"
        );
        let kept = list(pool, None, 10, 0).await.expect("the page reads");
        assert!(
            !kept.iter().any(|url| url.ends_with("a.png")),
            "the oldest row went first: {kept:?}"
        );
        assert!(
            kept.iter().any(|url| url.ends_with("c.png")),
            "the newest row stayed: {kept:?}"
        );

        // A refreshed URL is the newest row and survives the next prune.
        record(pool, "https://remote.example/b.png", 2)
            .await
            .expect("recording again refreshes");
        record(pool, "https://remote.example/d.png", 2)
            .await
            .expect("the fifth record prunes b's predecessor");
        let kept_after_refresh = list(pool, None, 10, 0).await.expect("the page reads");
        assert!(
            kept_after_refresh.iter().any(|url| url.ends_with("b.png")),
            "re-serving b made it younger than c: {kept_after_refresh:?}"
        );
        assert!(
            kept_after_refresh.iter().any(|url| url.ends_with("d.png")),
            "and d is the newest of all: {kept_after_refresh:?}"
        );
    }

    #[tokio::test]
    async fn list_filters_by_substring_pages_and_orders() {
        let db = test_db::test_db().await.expect("test database slot");
        let pool = db.pool();

        for url in [
            "https://remote.example/one.png",
            "https://remote.example/two.png",
            "https://other.example/onetwo.png",
        ] {
            seed(pool, url).await;
        }

        let all = list(pool, None, 10, 0).await.expect("the full page reads");
        assert_eq!(
            all,
            vec![
                "https://other.example/onetwo.png",
                "https://remote.example/one.png",
                "https://remote.example/two.png",
            ],
            "url order, everything unfiltered"
        );
        let page = list(pool, None, 2, 1).await.expect("a middle page reads");
        assert_eq!(
            page,
            vec![
                "https://remote.example/one.png",
                "https://remote.example/two.png",
            ],
            "offset 2/1 slices the window"
        );

        let one = list(pool, Some("one"), 10, 0)
            .await
            .expect("the filtered page reads");
        assert_eq!(
            one,
            vec![
                "https://other.example/onetwo.png",
                "https://remote.example/one.png",
            ],
            "substring, not prefix: onetwo matches too"
        );
        assert_eq!(
            count(pool, Some("one")).await.expect("the filtered count"),
            2,
            "the count sees the whole filtered set, not one page"
        );
        assert_eq!(
            count(pool, Some("ONE")).await.expect("case folds"),
            2,
            "the filter is case-insensitive, Pleroma's /i regex"
        );
        let wildcards = list(pool, Some("%"), 10, 0)
            .await
            .expect("a percent query reads");
        assert!(
            wildcards.is_empty(),
            "% is a literal character, not a wildcard"
        );

        let miss = list(pool, Some("nowhere"), 10, 0)
            .await
            .expect("a missing filter reads");
        assert!(miss.is_empty(), "nothing matches");
        assert_eq!(
            count(pool, Some("nowhere")).await.expect("and counts"),
            0,
            "the total is zero too"
        );
    }

    #[tokio::test]
    async fn remove_drops_the_named_urls_only() {
        let db = test_db::test_db().await.expect("test database slot");
        let pool = db.pool();

        seed(pool, "https://remote.example/a.png").await;
        seed(pool, "https://remote.example/b.png").await;

        let doomed = vec!["https://remote.example/a.png".to_owned()];
        assert_eq!(
            remove(pool, &doomed).await.expect("the delete runs"),
            1,
            "the stored row went"
        );
        assert_eq!(
            remove(pool, &doomed).await.expect("a repeat runs"),
            0,
            "and the second find nothing"
        );
        let never = vec!["https://nowhere.example/x.png".to_owned()];
        assert_eq!(
            remove(pool, &never).await.expect("an unknown url runs"),
            0,
            "a name that was never recorded affects nothing"
        );
        assert_eq!(
            list(pool, None, 10, 0).await.expect("what is left"),
            vec!["https://remote.example/b.png"],
            "only the untouched row stands"
        );
    }
}
