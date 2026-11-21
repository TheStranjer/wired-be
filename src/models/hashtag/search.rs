//! The prefix search over the stored tag names: the read behind the combined
//! search's hashtag arm ([`crate::api::search`]).
//!
//! [`search_prefix`] normalizes its needle with the module's
//! `normalize_name` and escapes it for `ILIKE` with [`like_prefix`], so a
//! search spells what the searcher typed — a prefix match on the stored,
//! lowercase name, never a substring one.

use sqlx::PgPool;

use crate::error::AppResult;

use super::{Hashtag, normalize_name};

/// The statement behind [`search_prefix`]: stored names begin-to-begin with
/// the escaped pattern, `ORDER BY name` pages them deterministically and
/// `LIMIT` cuts the page — the shape the combined search's hashtag arm
/// answers with ([`crate::api::search`]).
const SEARCH_PREFIX_SQL: &str = "SELECT * FROM hashtags WHERE name ILIKE $1 ORDER BY name LIMIT $2";

/// The tag rows whose stored name starts with the query `needle`, in name
/// order, at most `limit` of them: the combined search's hashtag arm
/// ([`crate::api::search`]).
///
/// The needle goes through this module's `normalize_name` first — trimmed,
/// leading `#`s dropped, lowercased — so a search for `#Rust` finds the
/// stored `rust`, and a needle that normalizes to nothing (or is longer
/// than the stored name bound) searches nothing. `ILIKE` keeps the fold
/// case-insensitive on the column side too; the pattern's `%`, `_` and
/// `\` are escaped so the needle matches what the searcher typed, and only
/// at the start: this is a prefix search, not a substring one (the
/// ticket's `'q%'`).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn search_prefix(pool: &PgPool, needle: &str, limit: i64) -> AppResult<Vec<Hashtag>> {
    let Some(tag) = normalize_name(needle) else {
        return Ok(Vec::new());
    };
    let hashtags = sqlx::query_as::<_, Hashtag>(SEARCH_PREFIX_SQL)
        .bind(like_prefix(&tag))
        .bind(limit.max(0))
        .fetch_all(pool)
        .await?;
    Ok(hashtags)
}

/// The `ILIKE` prefix pattern of a normalized tag name: the metacharacters
/// it may hold (`%`, `_`, and the escape character itself) are
/// backslash-escaped so a search spells what was typed, and the trailing
/// `%` makes it the prefix match [`search_prefix`] promises.
fn like_prefix(needle: &str) -> String {
    let mut pattern = String::with_capacity(needle.len() + 1);
    for symbol in needle.chars() {
        if matches!(symbol, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(symbol);
    }
    pattern.push('%');
    pattern
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::hashtag::{find_or_create, test_support::*},
        util::entities::MAX_HASHTAG_LEN,
    };

    #[tokio::test]
    async fn search_prefix_matches_the_start_of_stored_names() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        for name in ["#rust", "#rustlang", "#Rusty", "#crust"] {
            let _tag = find_or_create(&pool, name)
                .await
                .expect("the fixture tag stores")
                .expect("the name is usable");
        }

        assert_eq!(
            names(&search_prefix(&pool, "rust", 20).await.unwrap()),
            vec!["rust", "rustlang", "rusty"],
            "the match is at the start only — `crust` does not answer — \
             and the page is in name order"
        );
        assert_eq!(
            names(&search_prefix(&pool, "RUSTY", 20).await.unwrap()),
            vec!["rusty"],
            "`ILIKE` folds the case whatever the needle's spelling"
        );
        assert!(
            search_prefix(&pool, "zz", 20).await.unwrap().is_empty(),
            "a prefix no name carries answers nothing"
        );
    }

    #[tokio::test]
    async fn a_query_is_normalized_escaped_and_bounded() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let _rust = find_or_create(&pool, "#Rust")
            .await
            .unwrap()
            .expect("rust stores");
        let _pct = find_or_create(&pool, "50%off")
            .await
            .unwrap()
            .expect("the percent name stores");
        let _uni = find_or_create(&pool, "日本語")
            .await
            .unwrap()
            .expect("the unicode name stores");

        assert_eq!(
            names(&search_prefix(&pool, "  #RUST ", 20).await.unwrap()),
            vec!["rust"],
            "trim, `#`-strip and the case fold before the match"
        );
        assert_eq!(
            names(&search_prefix(&pool, "50%", 20).await.unwrap()),
            vec!["50%off"],
            "a literal percent in the needle does not act as a wildcard"
        );
        assert_eq!(
            names(&search_prefix(&pool, "日本", 20).await.unwrap()),
            vec!["日本語"],
            "and a unicode prefix matches its tag"
        );
        for blank in ["", "  ", "#", "###"] {
            assert!(
                search_prefix(&pool, blank, 20).await.unwrap().is_empty(),
                "`{blank:?}` normalizes to nothing and searches nothing"
            );
        }
        assert!(
            search_prefix(&pool, &"t".repeat(MAX_HASHTAG_LEN + 1), 20)
                .await
                .unwrap()
                .is_empty(),
            "a needle past the stored-name bound can only overshoot"
        );
        assert_eq!(
            names(&search_prefix(&pool, "ru", 1).await.unwrap()),
            vec!["rust"],
            "the limit cuts the name-ordered page"
        );
    }

    #[test]
    fn the_prefix_pattern_escapes_the_like_metacharacters() {
        assert_eq!(like_prefix("rust"), "rust%", "the plain case");
        assert_eq!(like_prefix("50%"), r"50\%%", "a percent stays literal");
        assert_eq!(like_prefix("a_b"), r"a\_b%", "an underscore stays literal");
        assert_eq!(
            like_prefix("a\\b"),
            r"a\\b%",
            "and so does the escape itself"
        );
    }
}
