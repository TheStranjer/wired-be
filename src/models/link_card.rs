//! The `link_cards` and `status_cards` tables: the cache of fetched rich
//! media preview cards and the statuses that render them.
//!
//! Reference: Pleroma's `Pleroma.Web.RichMediaCard`
//! (`lib/pleroma/web/rich_media/card.ex`, with the parsers under
//! `lib/pleroma/web/rich_media/`); its URL-keyed `rich_media_cards` table
//! is what `link_cards` is here, and Pleroma's per-activity card
//! association is what `status_cards` is. Decision 11 of
//! `docs/ARCHITECTURE.md` keeps this cache in Postgres rather than a
//! process-local store, so every worker renders from the same rows, and
//! decision 3 gives the table pair this module.
//!
//! `link_cards` is the cache, one row per URL: [`store`] writes (or
//! refreshes) a card in place, bumping `fetched_at`, and a `failed` status
//! row is how a dead URL stays remembered instead of being re-fetched on
//! every render. A card is shared by every status that links its URL —
//! the cache key is the URL, not the pair.
//!
//! `status_cards` holds one link per status ([`link_status`], idempotent
//! on the status primary key), which is also what deletes itself with
//! either side: the status's cascade on `statuses`, the card URL's cascade
//! on `link_cards`.
//!
//! This module is the whole storage layer; the fetcher that fills it, the
//! job that retries `failed` rows and the entity that renders a card all
//! belong to later tickets.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

/// [`get`]: one URL's cached card.
const GET_SQL: &str = "SELECT * FROM link_cards WHERE url = $1";

/// [`store`]: one card per URL — a first write inserts, every later one
/// rewrites the same row in place and restamps `fetched_at`, which is how
/// a re-fetch (successful or not) refreshes its cache entry.
const STORE_SQL: &str = "INSERT INTO link_cards (url, status, card)
     VALUES ($1, $2, $3)
        ON CONFLICT (url) DO UPDATE
        SET status = EXCLUDED.status, card = EXCLUDED.card, fetched_at = now()";

/// [`link_status`]: a status renders one card, so the status id is the
/// key a repeat link conflicts on — and `DO NOTHING` keeps the card the
/// status already shows.
const LINK_STATUS_SQL: &str = "INSERT INTO status_cards (status_id, url)
     VALUES ($1, $2)
        ON CONFLICT (status_id) DO NOTHING";

/// [`for_statuses`]: the joined card of every asked-for status, one
/// statement for a whole page (the batched read shape of
/// [`crate::services::render_status`]'s loaders). The join can only reach
/// rows whose URL is still cached, so an evicted card is simply absent.
const FOR_STATUSES_SQL: &str = "SELECT sc.status_id, lc.* FROM status_cards sc
      JOIN link_cards lc ON lc.url = sc.url
     WHERE sc.status_id = ANY($1)";

/// One row of `link_cards`: the card cached for one URL, together with
/// whether the fetch that produced it succeeded.
#[derive(Debug, sqlx::FromRow)]
pub struct LinkCard {
    /// The URL the card was fetched from, verbatim — the cache key.
    pub url: String,
    /// The outcome of the fetch that wrote this row: `ok` or `failed`,
    /// the two states the column's check constraint allows.
    pub status: String,
    /// The parsed card as JSON (title, image, favicon and the like);
    /// `None` for a failed fetch or a page that yielded no card.
    pub card: Option<Value>,
    /// When this row was last (re)fetched; every [`store`] restamps it.
    pub fetched_at: DateTime<Utc>,
}

/// The joined row of [`for_statuses`]: one status link with the [`LinkCard`]
/// its URL names, flattened — the shape of [`FOR_STATUSES_SQL`].
#[derive(Debug, sqlx::FromRow)]
struct StatusCardRow {
    /// The status this row links, from `status_cards.status_id`.
    status_id: Uuid,
    /// The card, from the joined `link_cards` row.
    #[sqlx(flatten)]
    card: LinkCard,
}

/// The card cached for `url`, `None` when the cache holds nothing for it.
///
/// A cached `failed` row is an answer, not an absence: it is how a dead
/// URL is remembered between renders.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get(pool: &PgPool, url: &str) -> AppResult<Option<LinkCard>> {
    let row = sqlx::query_as::<_, LinkCard>(GET_SQL)
        .bind(url)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Caches the outcome of fetching `url`'s card, inserting the row or
/// refreshing the one already there.
///
/// `status` must be `ok` or `failed` — the column's check constraint
/// refuses anything else with a database error, which this layer reports
/// unchanged. `card` is the parsed card's JSON, `None` when the fetch
/// yielded nothing. A repeat store rewrites status and card in place and
/// restamps `fetched_at` to `now()`.
///
/// # Errors
///
/// Any database failure (connection, encoding, constraint) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn store(pool: &PgPool, url: &str, status: &str, card: Option<&Value>) -> AppResult<()> {
    sqlx::query(STORE_SQL)
        .bind(url)
        .bind(status)
        .bind(card)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// Points the status `status_id` at the card cached for `url`.
///
/// A status renders one card and the link keeps the card the status
/// already shows: a repeat link — the same URL or another — changes
/// nothing, which makes re-rendering a status idempotent.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError);
/// an unknown `status_id` or a `url` the cache does not hold arrives as
/// the foreign-key violation the database reports.
pub async fn link_status(pool: &PgPool, status_id: Uuid, url: &str) -> AppResult<()> {
    sqlx::query(LINK_STATUS_SQL)
        .bind(status_id)
        .bind(url)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The card of every status in `ids`, keyed by status id.
///
/// Statuses that link no card are absent from the map, and an empty `ids`
/// answers the empty map; this is the batched read that renders a page of
/// statuses with one query instead of one per row.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn for_statuses(pool: &PgPool, ids: &[Uuid]) -> AppResult<HashMap<Uuid, LinkCard>> {
    let ids = ids.to_vec();
    let rows = sqlx::query_as::<_, StatusCardRow>(FOR_STATUSES_SQL)
        .bind(&ids)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.status_id, row.card))
        .collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        error::AppError,
        models::{
            status::{NewStatus, Status, insert as insert_status},
            user::{NewUser, User, insert as insert_user, local_ap_id},
        },
    };

    /// The instance origin the fixtures post under.
    const BASE: &str = TEST_BASE_URL;

    /// A local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
        )
        .await
        .expect("the fixture user inserts")
    }

    /// A local status by `author`.
    async fn status(pool: &PgPool, author: &User) -> Status {
        insert_status(
            pool,
            BASE,
            &NewStatus::local(author, "a link https://example.test"),
        )
        .await
        .expect("the fixture status inserts")
    }

    /// Whether `err` is the database's own foreign-key violation (`23503`).
    fn is_foreign_key_violation(err: &AppError) -> bool {
        matches!(err, AppError::Database(sqlx::Error::Database(db))
            if db.code().as_deref() == Some("23503"))
    }

    /// The `status_cards` rows of `url`, as status ids — what a no-write
    /// assertion compares against.
    async fn linked_statuses(pool: &PgPool, url: &str) -> Vec<Uuid> {
        sqlx::query_as::<_, (Uuid,)>("SELECT status_id FROM status_cards WHERE url = $1")
            .bind(url)
            .fetch_all(pool)
            .await
            .expect("the link table reads")
            .into_iter()
            .map(|row| row.0)
            .collect()
    }

    /// store writes a row and get answers it back, columns verbatim.
    #[tokio::test]
    async fn store_and_get_round_trip() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let card = json!({"title": "Example", "image": "https://example.test/i.png"});

        store(&pool, "https://example.test", "ok", Some(&card))
            .await
            .expect("an ok card stores");

        let fetched = get(&pool, "https://example.test")
            .await
            .expect("the cached card reads")
            .expect("the row is there");
        assert_eq!(fetched.url, "https://example.test", "the url key");
        assert_eq!(fetched.status, "ok", "the outcome");
        assert_eq!(fetched.card, Some(card), "the card JSON round trips");
        assert!(
            fetched.fetched_at <= Utc::now(),
            "the fresh stamp is the store's now()"
        );

        // A failed fetch caches too, with no card behind it.
        store(&pool, "https://dead.test", "failed", None)
            .await
            .expect("a failed fetch stores");
        let dead = get(&pool, "https://dead.test")
            .await
            .expect("the failed row reads")
            .expect("a failure is remembered, not absent");
        assert_eq!(dead.status, "failed", "the failed outcome");
        assert_eq!(dead.card, None, "and there is no card");

        assert!(
            get(&pool, "https://never-fetched.test")
                .await
                .expect("reading nothing is not a failure")
                .is_none(),
            "a URL the cache never held reads as absent"
        );
    }

    /// A repeat store rewrites status and card in place and bumps
    /// `fetched_at` back to the present.
    #[tokio::test]
    async fn store_upserts_and_bumps_fetched_at() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let first = json!({"title": "First"});
        store(&pool, "https://example.test", "failed", None)
            .await
            .expect("the fixture stores");

        // Age the row so the bump `store` must cause is observable.
        let epoch: DateTime<Utc> = "2000-01-01T00:00:00Z"
            .parse()
            .expect("a fixed past timestamp");
        let aged = sqlx::query("UPDATE link_cards SET fetched_at = $2 WHERE url = $1")
            .bind("https://example.test")
            .bind(epoch)
            .execute(&pool)
            .await
            .expect("the fixture ages the row")
            .rows_affected();
        assert_eq!(aged, 1, "the cached row was there to age");

        store(&pool, "https://example.test", "ok", Some(&first))
            .await
            .expect("the retry stores");
        let refreshed = get(&pool, "https://example.test")
            .await
            .expect("the card reads")
            .expect("the row survived the upsert");
        assert_eq!(refreshed.status, "ok", "the status is overwritten");
        assert_eq!(refreshed.card, Some(first), "and the card is replaced");
        assert!(
            refreshed.fetched_at > epoch,
            "and fetched_at is bumped back to now()"
        );

        // A later store with no card clears the old one too.
        store(&pool, "https://example.test", "failed", None)
            .await
            .expect("the second retry stores");
        let cleared = get(&pool, "https://example.test")
            .await
            .expect("the card reads")
            .expect("the row stands");
        assert_eq!(cleared.status, "failed", "ok back to failed");
        assert_eq!(cleared.card, None, "the gone card is not kept");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM link_cards")
                .fetch_one(&pool)
                .await
                .expect("the cache counts"),
            1,
            "the upsert rewrote in place, never added a row"
        );
    }

    /// The table's check constraint refuses a status outside ok/failed.
    #[tokio::test]
    async fn store_rejects_a_status_outside_the_two_states() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        store(&pool, "https://example.test", "ok", None)
            .await
            .expect("the fixture stores");

        let err = store(&pool, "https://example.test", "pending", None)
            .await
            .expect_err("a third state is not storable");
        assert!(
            matches!(err, AppError::Database(sqlx::Error::Database(_))),
            "the check violation arrives as a database error: {err}"
        );
        assert_eq!(
            get(&pool, "https://example.test")
                .await
                .expect("the row reads")
                .expect("the row stands")
                .status,
            "ok",
            "and the refused store changed nothing"
        );
    }

    /// [`link_status`] plus [`for_statuses`]: the keyed map of a batch, statuses
    /// without a link absent, empty input the empty map.
    #[tokio::test]
    async fn link_status_and_for_statuses_key_by_status() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let hers = status(&pool, &alice).await;
        let _hers_too = status(&pool, &alice).await;
        let bob = user(&pool, "bob").await;
        let his = status(&pool, &bob).await;
        let _untouched = status(&pool, &bob).await;

        let card = json!({"title": "Example"});
        store(&pool, "https://example.test", "ok", Some(&card))
            .await
            .expect("the fixture card stores");
        store(
            &pool,
            "https://other.test",
            "ok",
            Some(&json!({"title": "Other"})),
        )
        .await
        .expect("the second card stores");
        link_status(&pool, hers.id, "https://example.test")
            .await
            .expect("her status links its card");
        link_status(&pool, his.id, "https://other.test")
            .await
            .expect("his status links his own card");

        let ids = [hers.id, his.id, Uuid::now_v7()];
        let cards = for_statuses(&pool, &ids)
            .await
            .expect("the batched read runs");
        assert_eq!(cards.len(), 2, "only the two linked statuses answer");
        let linked = cards
            .get(&hers.id)
            .expect("her status is keyed by its own id");
        assert_eq!(linked.url, "https://example.test", "her card is hers");
        assert_eq!(linked.card.as_ref(), Some(&card), "with its JSON");
        assert_eq!(
            cards
                .get(&his.id)
                .expect("his status is keyed by its own id")
                .url,
            "https://other.test",
            "and his card is keyed under his id, not hers"
        );

        // A status that links nothing is absent, and asking for nothing
        // answers the empty map without touching the table.
        let none = for_statuses(&pool, &[])
            .await
            .expect("the empty batch runs");
        assert!(none.is_empty(), "no ids, no cards");
    }

    /// A duplicate link — the same URL twice or a different one — changes
    /// nothing: the status keeps the card it first showed.
    #[tokio::test]
    async fn link_status_is_idempotent() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let post = status(&pool, &alice).await;
        store(
            &pool,
            "https://first.test",
            "ok",
            Some(&json!({"title": "First"})),
        )
        .await
        .expect("the first card stores");
        store(
            &pool,
            "https://second.test",
            "ok",
            Some(&json!({"title": "Second"})),
        )
        .await
        .expect("the second card stores");

        link_status(&pool, post.id, "https://first.test")
            .await
            .expect("the first link writes");
        link_status(&pool, post.id, "https://first.test")
            .await
            .expect("a repeat link is no failure");
        link_status(&pool, post.id, "https://second.test")
            .await
            .expect("and neither is another URL's link");

        let cards = for_statuses(&pool, &[post.id])
            .await
            .expect("the batched read runs");
        assert_eq!(cards.len(), 1, "one status, exactly one link row");
        assert_eq!(
            linked_statuses(&pool, "https://first.test").await,
            vec![post.id],
            "the first card still stands behind the status"
        );
        assert!(
            linked_statuses(&pool, "https://second.test")
                .await
                .is_empty(),
            "and the refused relink wrote nothing"
        );
    }

    /// Linking a status that does not exist (or a URL the cache does not
    /// hold) is the database's foreign-key violation, reported unchanged.
    #[tokio::test]
    async fn link_status_requires_both_sides_to_exist() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let post = status(&pool, &alice).await;

        let err = link_status(&pool, Uuid::now_v7(), "https://example.test")
            .await
            .expect_err("a missing status cannot be linked");
        assert!(is_foreign_key_violation(&err), "the key refuses: {err}");

        // The URL side is keyed too: nothing cached, nothing to link.
        let url_err = link_status(&pool, post.id, "https://uncached.test")
            .await
            .expect_err("a missing card cannot be linked");
        assert!(
            is_foreign_key_violation(&url_err),
            "the key refuses: {url_err}"
        );

        assert!(
            linked_statuses(&pool, "https://uncached.test")
                .await
                .is_empty(),
            "and no attempt wrote a link row"
        );
    }
}
