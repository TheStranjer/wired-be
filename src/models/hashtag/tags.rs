//! The `hashtags` rows: the idempotent, case-insensitive upsert and the
//! reads one stored name answers.
//!
//! [`upsert_many`] is the module's only writer of a tag row: every name goes
//! through `normalize_name` first, unusable names are dropped, repeats within
//! one call are folded, and a name the table already holds answers its
//! stored row instead of raising the unique violation. Its connection face,
//! `upsert_many_conn`, is what [`crate::models::status::insert`] writes a
//! status's tags through, inside the status's own transaction.
//! [`find_or_create`] and [`get_by_name`] are the single-name reads of the
//! rows those writes leave behind.

use std::collections::HashSet;

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

use super::{Hashtag, normalize_name};

/// The statement behind [`upsert_many`]: one row per given `(id, name)`
/// pair, and on a name that already exists the no-op update of
/// `name = EXCLUDED.name` — the shape the ticket prescribes, and what makes
/// `RETURNING` answer with the stored row (its original `id` and
/// `inserted_at`) instead of raising the unique violation.
const UPSERT_SQL: &str = "INSERT INTO hashtags (id, name)
     SELECT u.id, u.name FROM unnest($1::uuid[], $2::text[]) AS u(id, name)
ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name
RETURNING id, name, inserted_at";

/// [`get_by_name`]: one row by its stored (normalized) name.
const BY_NAME_SQL: &str = "SELECT * FROM hashtags WHERE name = $1";

/// Upserts every usable name in `names` and returns the rows they name,
/// ordered by name.
///
/// A name is normalized (trimmed, `#`-stripped, lowercased) before it is
/// looked at, so `#Foo`, `Foo` and `foo` are one tag; a name that
/// normalizes to nothing or to more than 100 characters (the extraction
/// bound, [`crate::util::entities::extract_hashtags`]) is ignored.
/// Idempotent: a name the table already holds answers its stored row, and
/// repeats within one call are folded to one write.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn upsert_many(pool: &PgPool, names: &[String]) -> AppResult<Vec<Hashtag>> {
    let mut conn = pool.acquire().await?;
    upsert_many_conn(&mut conn, names).await
}

/// [`upsert_many`] on an open connection: the statement runs on the
/// caller's transaction, which is how [`crate::models::status::insert`]
/// writes a status and its tags together.
///
/// An empty batch short-circuits: no statement, no round trip.
pub(super) async fn upsert_many_conn(
    conn: &mut PgConnection,
    names: &[String],
) -> AppResult<Vec<Hashtag>> {
    let names = normalized_names(names);
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = names.iter().map(|_| new_id()).collect();
    let mut hashtags = sqlx::query_as::<_, Hashtag>(UPSERT_SQL)
        .bind(&ids)
        .bind(&names)
        .fetch_all(&mut *conn)
        .await?;
    // The rows come back in whatever order the statement folded them; the
    // name order is what callers (and tests) get to rely on.
    hashtags.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok(hashtags)
}

/// Finds or creates the one tag `name` names.
///
/// The single-name face of [`upsert_many`]: `#Foo` finds `foo`, and an
/// unknown usable name gets a row. A name that normalization rejects
/// (blank, or longer than the 100-character stored-name bound) is `None` —
/// there is no tag to find and none to store.
///
/// # Errors
///
/// Any database failure behind the upsert (connection, encoding) arrives
/// as [`AppError::Database`](crate::error::AppError).
pub async fn find_or_create(pool: &PgPool, name: &str) -> AppResult<Option<Hashtag>> {
    let mut usable: Vec<String> = Vec::new();
    if let Some(tag) = normalize_name(name) {
        usable.push(tag);
    }
    let mut found = upsert_many(pool, &usable).await?;
    Ok(found.pop())
}

/// The stored tag whose name normalizes to `name`'s, `None` when no row
/// holds it — including when `name` is not a usable tag at all.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_name(pool: &PgPool, name: &str) -> AppResult<Option<Hashtag>> {
    let Some(tag) = normalize_name(name) else {
        return Ok(None);
    };
    let hashtag = sqlx::query_as::<_, Hashtag>(BY_NAME_SQL)
        .bind(&tag)
        .fetch_optional(pool)
        .await?;
    Ok(hashtag)
}

/// `names` normalized, unusable names dropped and repeats folded to their
/// first occurrence, in order: what [`UPSERT_SQL`] inserts, so one call
/// never offers the same name twice (which `ON CONFLICT DO UPDATE` refuses
/// to fold inside a single statement).
fn normalized_names(names: &[String]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut usable: Vec<String> = Vec::new();
    for name in names {
        let Some(tag) = normalize_name(name) else {
            continue;
        };
        if seen.insert(tag.clone()) {
            usable.push(tag);
        }
    }
    usable
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{models::hashtag::test_support::*, util::entities::MAX_HASHTAG_LEN};

    #[tokio::test]
    async fn upsert_is_idempotent_and_case_insensitive() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let first = upsert_many(&pool, &tags(&["Foo"]))
            .await
            .expect("the first upsert stores");
        assert_eq!(names(&first), vec!["foo"], "the name is stored lowercase");

        let again = upsert_many(&pool, &tags(&["foo", "#FOO", " #Foo ", "FOO"]))
            .await
            .expect("a repeat upsert runs");
        assert_eq!(
            again, first,
            "every spelling answers the one stored row, folds included"
        );
        assert_eq!(
            stored_tags(&pool).await,
            vec![("foo".to_owned(), first[0].id)],
            "and the table still holds exactly one tag"
        );
    }

    #[tokio::test]
    async fn upsert_keeps_order_by_name_and_drops_the_unusable() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let long = "t".repeat(MAX_HASHTAG_LEN + 1);
        let stored = upsert_many(
            &pool,
            &tags(&["beta", &long, "", "#", "  ", "ALPHA", "beta"]),
        )
        .await
        .expect("a mixed batch runs");
        assert_eq!(
            names(&stored),
            vec!["alpha", "beta"],
            "results come ordered by name"
        );
        assert_eq!(
            stored_tags(&pool).await.len(),
            2,
            "the over-long, blank and hash-only names were ignored"
        );

        let exactly_max = "x".repeat(MAX_HASHTAG_LEN);
        let kept = upsert_many(&pool, &tags(&[&exactly_max]))
            .await
            .expect("a 100-character name is usable");
        assert_eq!(
            names(&kept),
            vec![exactly_max.as_str()],
            "the bound itself is kept, only longer is ignored"
        );
    }

    #[tokio::test]
    async fn find_or_create_and_get_by_name_agree() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        assert!(
            get_by_name(&pool, "rust").await.unwrap().is_none(),
            "an unknown name is no tag yet"
        );
        let created = find_or_create(&pool, "#Rust")
            .await
            .expect("creating works")
            .expect("the name is usable");
        assert_eq!(created.name, "rust", "the row is the normalized tag");

        let found = find_or_create(&pool, "rust")
            .await
            .expect("finding works")
            .expect("the name is usable");
        assert_eq!(found, created, "and answers the very same row");
        assert_eq!(
            get_by_name(&pool, "RUST")
                .await
                .unwrap()
                .expect("get_by_name normalizes too"),
            created,
            "any spelling of the name finds it"
        );
        assert!(
            find_or_create(&pool, "   ").await.unwrap().is_none(),
            "a blank name is nothing to find or create"
        );
        assert!(
            get_by_name(&pool, &"t".repeat(MAX_HASHTAG_LEN + 1))
                .await
                .unwrap()
                .is_none(),
            "an over-long name is nothing to find"
        );
    }
}
